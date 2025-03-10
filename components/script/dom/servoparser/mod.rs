/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::borrow::Cow;
use std::cell::Cell;
use std::mem;

use base64::engine::general_purpose;
use base64::Engine as _;
use content_security_policy::{self as csp, CspList};
use dom_struct::dom_struct;
use embedder_traits::resources::{self, Resource};
use encoding_rs::Encoding;
use html5ever::buffer_queue::BufferQueue;
use html5ever::tendril::fmt::UTF8;
use html5ever::tendril::{ByteTendril, StrTendril, TendrilSink};
use html5ever::tokenizer::TokenizerResult;
use html5ever::tree_builder::{ElementFlags, NextParserState, NodeOrText, QuirksMode, TreeSink};
use html5ever::{local_name, namespace_url, ns, Attribute, ExpandedName, LocalName, QualName};
use hyper_serde::Serde;
use mime::{self, Mime};
use msg::constellation_msg::PipelineId;
use net_traits::{
    FetchMetadata, FetchResponseListener, Metadata, NetworkError, ResourceFetchTiming,
    ResourceTimingType,
};
use profile_traits::time::{
    profile, ProfilerCategory, TimerMetadata, TimerMetadataFrameType, TimerMetadataReflowType,
};
use script_traits::DocumentActivity;
use servo_config::pref;
use servo_url::ServoUrl;
use style::context::QuirksMode as ServoQuirksMode;
use tendril::stream::LossyDecoder;

use crate::document_loader::{DocumentLoader, LoadType};
use crate::dom::bindings::cell::DomRefCell;
use crate::dom::bindings::codegen::Bindings::DocumentBinding::{
    DocumentMethods, DocumentReadyState,
};
use crate::dom::bindings::codegen::Bindings::HTMLImageElementBinding::HTMLImageElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLTemplateElementBinding::HTMLTemplateElementMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::{reflect_dom_object, DomObject, Reflector};
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::settings_stack::is_execution_stack_empty;
use crate::dom::bindings::str::{DOMString, USVString};
use crate::dom::characterdata::CharacterData;
use crate::dom::comment::Comment;
use crate::dom::document::{Document, DocumentSource, HasBrowsingContext, IsHTMLDocument};
use crate::dom::documenttype::DocumentType;
use crate::dom::element::{CustomElementCreationMode, Element, ElementCreator};
use crate::dom::globalscope::GlobalScope;
use crate::dom::htmlformelement::{FormControlElementHelpers, HTMLFormElement};
use crate::dom::htmlimageelement::HTMLImageElement;
use crate::dom::htmlinputelement::HTMLInputElement;
use crate::dom::htmlscriptelement::{HTMLScriptElement, ScriptResult};
use crate::dom::htmltemplateelement::HTMLTemplateElement;
use crate::dom::node::{Node, ShadowIncluding};
use crate::dom::performanceentry::PerformanceEntry;
use crate::dom::performancenavigationtiming::PerformanceNavigationTiming;
use crate::dom::processinginstruction::ProcessingInstruction;
use crate::dom::text::Text;
use crate::dom::virtualmethods::vtable_for;
use crate::network_listener::PreInvoke;
use crate::realms::enter_realm;
use crate::script_thread::ScriptThread;

mod async_html;
mod html;
mod prefetch;
mod xml;

#[dom_struct]
/// The parser maintains two input streams: one for input from script through
/// document.write(), and one for input from network.
///
/// There is no concrete representation of the insertion point, instead it
/// always points to just before the next character from the network input,
/// with all of the script input before itself.
///
/// ```text
///     ... script input ... | ... network input ...
///                          ^
///                 insertion point
/// ```
pub struct ServoParser {
    reflector: Reflector,
    /// The document associated with this parser.
    document: Dom<Document>,
    /// The BOM sniffing state.
    ///
    /// `None` means we've found the BOM, we've found there isn't one, or
    /// we're not parsing from a byte stream. `Some` contains the BOM bytes
    /// found so far.
    bom_sniff: DomRefCell<Option<Vec<u8>>>,
    /// The decoder used for the network input.
    network_decoder: DomRefCell<Option<NetworkDecoder>>,
    /// Input received from network.
    #[ignore_malloc_size_of = "Defined in html5ever"]
    #[no_trace]
    network_input: DomRefCell<BufferQueue>,
    /// Input received from script. Used only to support document.write().
    #[ignore_malloc_size_of = "Defined in html5ever"]
    #[no_trace]
    script_input: DomRefCell<BufferQueue>,
    /// The tokenizer of this parser.
    tokenizer: DomRefCell<Tokenizer>,
    /// Whether to expect any further input from the associated network request.
    last_chunk_received: Cell<bool>,
    /// Whether this parser should avoid passing any further data to the tokenizer.
    suspended: Cell<bool>,
    /// <https://html.spec.whatwg.org/multipage/#script-nesting-level>
    script_nesting_level: Cell<usize>,
    /// <https://html.spec.whatwg.org/multipage/#abort-a-parser>
    aborted: Cell<bool>,
    /// <https://html.spec.whatwg.org/multipage/#script-created-parser>
    script_created_parser: bool,
    /// We do a quick-and-dirty parse of the input looking for resources to prefetch.
    // TODO: if we had speculative parsing, we could do this when speculatively
    // building the DOM. https://github.com/servo/servo/pull/19203
    prefetch_tokenizer: DomRefCell<prefetch::Tokenizer>,
    #[ignore_malloc_size_of = "Defined in html5ever"]
    #[no_trace]
    prefetch_input: DomRefCell<BufferQueue>,
}

#[derive(PartialEq)]
enum LastChunkState {
    Received,
    NotReceived,
}

pub struct ElementAttribute {
    name: QualName,
    value: DOMString,
}

#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
pub enum ParsingAlgorithm {
    Normal,
    Fragment,
}

impl ElementAttribute {
    pub fn new(name: QualName, value: DOMString) -> ElementAttribute {
        ElementAttribute {
            name: name,
            value: value,
        }
    }
}

impl ServoParser {
    pub fn parser_is_not_active(&self) -> bool {
        self.can_write() || self.tokenizer.try_borrow_mut().is_ok()
    }

    pub fn parse_html_document(document: &Document, input: Option<DOMString>, url: ServoUrl) {
        let parser = if pref!(dom.servoparser.async_html_tokenizer.enabled) {
            ServoParser::new(
                document,
                Tokenizer::AsyncHtml(self::async_html::Tokenizer::new(document, url, None)),
                LastChunkState::NotReceived,
                ParserKind::Normal,
            )
        } else {
            ServoParser::new(
                document,
                Tokenizer::Html(self::html::Tokenizer::new(
                    document,
                    url,
                    None,
                    ParsingAlgorithm::Normal,
                )),
                LastChunkState::NotReceived,
                ParserKind::Normal,
            )
        };

        // Set as the document's current parser and initialize with `input`, if given.
        if let Some(input) = input {
            parser.parse_string_chunk(String::from(input));
        } else {
            parser.document.set_current_parser(Some(&parser));
        }
    }

    // https://html.spec.whatwg.org/multipage/#parsing-html-fragments
    pub fn parse_html_fragment(
        context: &Element,
        input: DOMString,
    ) -> impl Iterator<Item = DomRoot<Node>> {
        let context_node = context.upcast::<Node>();
        let context_document = context_node.owner_doc();
        let window = context_document.window();
        let url = context_document.url();

        // Step 1.
        let loader = DocumentLoader::new_with_threads(
            context_document.loader().resource_threads().clone(),
            Some(url.clone()),
        );
        let document = Document::new(
            window,
            HasBrowsingContext::No,
            Some(url.clone()),
            context_document.origin().clone(),
            IsHTMLDocument::HTMLDocument,
            None,
            None,
            DocumentActivity::Inactive,
            DocumentSource::FromParser,
            loader,
            None,
            None,
            Default::default(),
        );

        // Step 2.
        document.set_quirks_mode(context_document.quirks_mode());

        // Step 11.
        let form = context_node
            .inclusive_ancestors(ShadowIncluding::No)
            .find(|element| element.is::<HTMLFormElement>());

        let fragment_context = FragmentContext {
            context_elem: context_node,
            form_elem: form.as_deref(),
        };

        let parser = ServoParser::new(
            &document,
            Tokenizer::Html(self::html::Tokenizer::new(
                &document,
                url,
                Some(fragment_context),
                ParsingAlgorithm::Fragment,
            )),
            LastChunkState::Received,
            ParserKind::Normal,
        );
        parser.parse_string_chunk(String::from(input));

        // Step 14.
        let root_element = document.GetDocumentElement().expect("no document element");
        FragmentParsingResult {
            inner: root_element.upcast::<Node>().children(),
        }
    }

    pub fn parse_html_script_input(document: &Document, url: ServoUrl) {
        let parser = ServoParser::new(
            document,
            Tokenizer::Html(self::html::Tokenizer::new(
                document,
                url,
                None,
                ParsingAlgorithm::Normal,
            )),
            LastChunkState::NotReceived,
            ParserKind::ScriptCreated,
        );
        *parser.bom_sniff.borrow_mut() = None;
        document.set_current_parser(Some(&parser));
    }

    pub fn parse_xml_document(document: &Document, input: Option<DOMString>, url: ServoUrl) {
        let parser = ServoParser::new(
            document,
            Tokenizer::Xml(self::xml::Tokenizer::new(document, url)),
            LastChunkState::NotReceived,
            ParserKind::Normal,
        );

        // Set as the document's current parser and initialize with `input`, if given.
        if let Some(input) = input {
            parser.parse_string_chunk(String::from(input));
        } else {
            parser.document.set_current_parser(Some(&parser));
        }
    }

    pub fn script_nesting_level(&self) -> usize {
        self.script_nesting_level.get()
    }

    pub fn is_script_created(&self) -> bool {
        self.script_created_parser
    }

    /// Corresponds to the latter part of the "Otherwise" branch of the 'An end
    /// tag whose tag name is "script"' of
    /// <https://html.spec.whatwg.org/multipage/#parsing-main-incdata>
    ///
    /// This first moves everything from the script input to the beginning of
    /// the network input, effectively resetting the insertion point to just
    /// before the next character to be consumed.
    ///
    ///
    /// ```text
    ///     | ... script input ... network input ...
    ///     ^
    ///     insertion point
    /// ```
    pub fn resume_with_pending_parsing_blocking_script(
        &self,
        script: &HTMLScriptElement,
        result: ScriptResult,
    ) {
        assert!(self.suspended.get());
        self.suspended.set(false);

        mem::swap(
            &mut *self.script_input.borrow_mut(),
            &mut *self.network_input.borrow_mut(),
        );
        while let Some(chunk) = self.script_input.borrow_mut().pop_front() {
            self.network_input.borrow_mut().push_back(chunk);
        }

        let script_nesting_level = self.script_nesting_level.get();
        assert_eq!(script_nesting_level, 0);

        self.script_nesting_level.set(script_nesting_level + 1);
        script.execute(result);
        self.script_nesting_level.set(script_nesting_level);

        if !self.suspended.get() {
            self.parse_sync();
        }
    }

    pub fn can_write(&self) -> bool {
        self.script_created_parser || self.script_nesting_level.get() > 0
    }

    /// Steps 6-8 of <https://html.spec.whatwg.org/multipage/#document.write()>
    pub fn write(&self, text: Vec<DOMString>) {
        assert!(self.can_write());

        if self.document.has_pending_parsing_blocking_script() {
            // There is already a pending parsing blocking script so the
            // parser is suspended, we just append everything to the
            // script input and abort these steps.
            for chunk in text {
                self.script_input
                    .borrow_mut()
                    .push_back(String::from(chunk).into());
            }
            return;
        }

        // There is no pending parsing blocking script, so all previous calls
        // to document.write() should have seen their entire input tokenized
        // and process, with nothing pushed to the parser script input.
        assert!(self.script_input.borrow().is_empty());

        let mut input = BufferQueue::new();
        for chunk in text {
            input.push_back(String::from(chunk).into());
        }

        self.tokenize(|tokenizer| tokenizer.feed(&mut input));

        if self.suspended.get() {
            // Parser got suspended, insert remaining input at end of
            // script input, following anything written by scripts executed
            // reentrantly during this call.
            while let Some(chunk) = input.pop_front() {
                self.script_input.borrow_mut().push_back(chunk);
            }
            return;
        }

        assert!(input.is_empty());
    }

    // Steps 4-6 of https://html.spec.whatwg.org/multipage/#dom-document-close
    pub fn close(&self) {
        assert!(self.script_created_parser);

        // Step 4.
        self.last_chunk_received.set(true);

        if self.suspended.get() {
            // Step 5.
            return;
        }

        // Step 6.
        self.parse_sync();
    }

    // https://html.spec.whatwg.org/multipage/#abort-a-parser
    pub fn abort(&self) {
        assert!(!self.aborted.get());
        self.aborted.set(true);

        // Step 1.
        *self.script_input.borrow_mut() = BufferQueue::new();
        *self.network_input.borrow_mut() = BufferQueue::new();

        // Step 2.
        self.document
            .set_ready_state(DocumentReadyState::Interactive);

        // Step 3.
        self.tokenizer.borrow_mut().end();
        self.document.set_current_parser(None);

        // Step 4.
        self.document.set_ready_state(DocumentReadyState::Complete);
    }

    // https://html.spec.whatwg.org/multipage/#active-parser
    pub fn is_active(&self) -> bool {
        self.script_nesting_level() > 0 && !self.aborted.get()
    }

    #[allow(crown::unrooted_must_root)]
    fn new_inherited(
        document: &Document,
        tokenizer: Tokenizer,
        last_chunk_state: LastChunkState,
        kind: ParserKind,
    ) -> Self {
        ServoParser {
            reflector: Reflector::new(),
            document: Dom::from_ref(document),
            bom_sniff: DomRefCell::new(Some(Vec::with_capacity(3))),
            network_decoder: DomRefCell::new(Some(NetworkDecoder::new(document.encoding()))),
            network_input: DomRefCell::new(BufferQueue::new()),
            script_input: DomRefCell::new(BufferQueue::new()),
            tokenizer: DomRefCell::new(tokenizer),
            last_chunk_received: Cell::new(last_chunk_state == LastChunkState::Received),
            suspended: Default::default(),
            script_nesting_level: Default::default(),
            aborted: Default::default(),
            script_created_parser: kind == ParserKind::ScriptCreated,
            prefetch_tokenizer: DomRefCell::new(prefetch::Tokenizer::new(document)),
            prefetch_input: DomRefCell::new(BufferQueue::new()),
        }
    }

    #[allow(crown::unrooted_must_root)]
    fn new(
        document: &Document,
        tokenizer: Tokenizer,
        last_chunk_state: LastChunkState,
        kind: ParserKind,
    ) -> DomRoot<Self> {
        reflect_dom_object(
            Box::new(ServoParser::new_inherited(
                document,
                tokenizer,
                last_chunk_state,
                kind,
            )),
            document.window(),
        )
    }

    fn push_tendril_input_chunk(&self, chunk: StrTendril) {
        if chunk.is_empty() {
            return;
        }
        // Per https://github.com/whatwg/html/issues/1495
        // stylesheets should not be loaded for documents
        // without browsing contexts.
        // https://github.com/whatwg/html/issues/1495#issuecomment-230334047
        // suggests that no content should be preloaded in such a case.
        // We're conservative, and only prefetch for documents
        // with browsing contexts.
        if self.document.browsing_context().is_some() {
            // Push the chunk into the prefetch input stream,
            // which is tokenized eagerly, to scan for resources
            // to prefetch. If the user script uses `document.write()`
            // to overwrite the network input, this prefetching may
            // have been wasted, but in most cases it won't.
            let mut prefetch_input = self.prefetch_input.borrow_mut();
            prefetch_input.push_back(chunk.clone());
            self.prefetch_tokenizer
                .borrow_mut()
                .feed(&mut *prefetch_input);
        }
        // Push the chunk into the network input stream,
        // which is tokenized lazily.
        self.network_input.borrow_mut().push_back(chunk);
    }

    fn push_bytes_input_chunk(&self, chunk: Vec<u8>) {
        // BOM sniff. This is needed because NetworkDecoder will switch the
        // encoding based on the BOM, but it won't change
        // `self.document.encoding` in the process.
        {
            let mut bom_sniff = self.bom_sniff.borrow_mut();
            if let Some(partial_bom) = bom_sniff.as_mut() {
                if partial_bom.len() + chunk.len() >= 3 {
                    partial_bom.extend(chunk.iter().take(3 - partial_bom.len()).copied());
                    if let Some((encoding, _)) = Encoding::for_bom(&partial_bom) {
                        self.document.set_encoding(encoding);
                    }
                    drop(bom_sniff);
                    *self.bom_sniff.borrow_mut() = None;
                } else {
                    partial_bom.extend(chunk.iter().copied());
                }
            }
        }

        // For byte input, we convert it to text using the network decoder.
        let chunk = self
            .network_decoder
            .borrow_mut()
            .as_mut()
            .unwrap()
            .decode(chunk);
        self.push_tendril_input_chunk(chunk);
    }

    fn push_string_input_chunk(&self, chunk: String) {
        // If the input is a string, we don't have a BOM.
        if self.bom_sniff.borrow().is_some() {
            *self.bom_sniff.borrow_mut() = None;
        }

        // The input has already been decoded as a string, so doesn't need
        // to be decoded by the network decoder again.
        let chunk = StrTendril::from(chunk);
        self.push_tendril_input_chunk(chunk);
    }

    fn parse_sync(&self) {
        let metadata = TimerMetadata {
            url: self.document.url().as_str().into(),
            iframe: TimerMetadataFrameType::RootWindow,
            incremental: TimerMetadataReflowType::FirstReflow,
        };
        let profiler_category = self.tokenizer.borrow().profiler_category();
        profile(
            profiler_category,
            Some(metadata),
            self.document
                .window()
                .upcast::<GlobalScope>()
                .time_profiler_chan()
                .clone(),
            || self.do_parse_sync(),
        )
    }

    fn do_parse_sync(&self) {
        assert!(self.script_input.borrow().is_empty());

        // This parser will continue to parse while there is either pending input or
        // the parser remains unsuspended.

        if self.last_chunk_received.get() {
            if let Some(decoder) = self.network_decoder.borrow_mut().take() {
                let chunk = decoder.finish();
                if !chunk.is_empty() {
                    self.network_input.borrow_mut().push_back(chunk);
                }
            }
        }
        self.tokenize(|tokenizer| tokenizer.feed(&mut *self.network_input.borrow_mut()));

        if self.suspended.get() {
            return;
        }

        assert!(self.network_input.borrow().is_empty());

        if self.last_chunk_received.get() {
            self.finish();
        }
    }

    fn parse_string_chunk(&self, input: String) {
        self.document.set_current_parser(Some(self));
        self.push_string_input_chunk(input);
        if !self.suspended.get() {
            self.parse_sync();
        }
    }

    fn parse_bytes_chunk(&self, input: Vec<u8>) {
        self.document.set_current_parser(Some(self));
        self.push_bytes_input_chunk(input);
        if !self.suspended.get() {
            self.parse_sync();
        }
    }

    fn tokenize<F>(&self, mut feed: F)
    where
        F: FnMut(&mut Tokenizer) -> TokenizerResult<DomRoot<HTMLScriptElement>>,
    {
        loop {
            assert!(!self.suspended.get());
            assert!(!self.aborted.get());

            self.document.reflow_if_reflow_timer_expired();
            let script = match feed(&mut *self.tokenizer.borrow_mut()) {
                TokenizerResult::Done => return,
                TokenizerResult::Script(script) => script,
            };

            // https://html.spec.whatwg.org/multipage/#parsing-main-incdata
            // branch "An end tag whose tag name is "script"
            // The spec says to perform the microtask checkpoint before
            // setting the insertion mode back from Text, but this is not
            // possible with the way servo and html5ever currently
            // relate to each other, and hopefully it is not observable.
            if is_execution_stack_empty() {
                self.document
                    .window()
                    .upcast::<GlobalScope>()
                    .perform_a_microtask_checkpoint();
            }

            let script_nesting_level = self.script_nesting_level.get();

            self.script_nesting_level.set(script_nesting_level + 1);
            script.prepare();
            self.script_nesting_level.set(script_nesting_level);

            if self.document.has_pending_parsing_blocking_script() {
                self.suspended.set(true);
                return;
            }
            if self.aborted.get() {
                return;
            }
        }
    }

    // https://html.spec.whatwg.org/multipage/#the-end
    fn finish(&self) {
        assert!(!self.suspended.get());
        assert!(self.last_chunk_received.get());
        assert!(self.script_input.borrow().is_empty());
        assert!(self.network_input.borrow().is_empty());
        assert!(self.network_decoder.borrow().is_none());

        // Step 1.
        self.document
            .set_ready_state(DocumentReadyState::Interactive);

        // Step 2.
        self.tokenizer.borrow_mut().end();
        self.document.set_current_parser(None);

        // Steps 3-12 are in another castle, namely finish_load.
        let url = self.tokenizer.borrow().url().clone();
        self.document.finish_load(LoadType::PageSource(url));
    }
}

struct FragmentParsingResult<I>
where
    I: Iterator<Item = DomRoot<Node>>,
{
    inner: I,
}

impl<I> Iterator for FragmentParsingResult<I>
where
    I: Iterator<Item = DomRoot<Node>>,
{
    type Item = DomRoot<Node>;

    fn next(&mut self) -> Option<DomRoot<Node>> {
        let next = self.inner.next()?;
        next.remove_self();
        Some(next)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

#[derive(JSTraceable, MallocSizeOf, PartialEq)]
enum ParserKind {
    Normal,
    ScriptCreated,
}

#[derive(JSTraceable, MallocSizeOf)]
#[crown::unrooted_must_root_lint::must_root]
enum Tokenizer {
    Html(self::html::Tokenizer),
    AsyncHtml(self::async_html::Tokenizer),
    Xml(self::xml::Tokenizer),
}

impl Tokenizer {
    #[must_use]
    fn feed(&mut self, input: &mut BufferQueue) -> TokenizerResult<DomRoot<HTMLScriptElement>> {
        match *self {
            Tokenizer::Html(ref mut tokenizer) => tokenizer.feed(input),
            Tokenizer::AsyncHtml(ref mut tokenizer) => tokenizer.feed(input),
            Tokenizer::Xml(ref mut tokenizer) => tokenizer.feed(input),
        }
    }

    fn end(&mut self) {
        match *self {
            Tokenizer::Html(ref mut tokenizer) => tokenizer.end(),
            Tokenizer::AsyncHtml(ref mut tokenizer) => tokenizer.end(),
            Tokenizer::Xml(ref mut tokenizer) => tokenizer.end(),
        }
    }

    fn url(&self) -> &ServoUrl {
        match *self {
            Tokenizer::Html(ref tokenizer) => tokenizer.url(),
            Tokenizer::AsyncHtml(ref tokenizer) => tokenizer.url(),
            Tokenizer::Xml(ref tokenizer) => tokenizer.url(),
        }
    }

    fn set_plaintext_state(&mut self) {
        match *self {
            Tokenizer::Html(ref mut tokenizer) => tokenizer.set_plaintext_state(),
            Tokenizer::AsyncHtml(ref mut tokenizer) => tokenizer.set_plaintext_state(),
            Tokenizer::Xml(_) => unimplemented!(),
        }
    }

    fn profiler_category(&self) -> ProfilerCategory {
        match *self {
            Tokenizer::Html(_) => ProfilerCategory::ScriptParseHTML,
            Tokenizer::AsyncHtml(_) => ProfilerCategory::ScriptParseHTML,
            Tokenizer::Xml(_) => ProfilerCategory::ScriptParseXML,
        }
    }
}

/// The context required for asynchronously fetching a document
/// and parsing it progressively.
pub struct ParserContext {
    /// The parser that initiated the request.
    parser: Option<Trusted<ServoParser>>,
    /// Is this a synthesized document
    is_synthesized_document: bool,
    /// The pipeline associated with this document.
    id: PipelineId,
    /// The URL for this document.
    url: ServoUrl,
    /// timing data for this resource
    resource_timing: ResourceFetchTiming,
    /// pushed entry index
    pushed_entry_index: Option<usize>,
}

impl ParserContext {
    pub fn new(id: PipelineId, url: ServoUrl) -> ParserContext {
        ParserContext {
            parser: None,
            is_synthesized_document: false,
            id: id,
            url: url,
            resource_timing: ResourceFetchTiming::new(ResourceTimingType::Navigation),
            pushed_entry_index: None,
        }
    }
}

impl FetchResponseListener for ParserContext {
    fn process_request_body(&mut self) {}

    fn process_request_eof(&mut self) {}

    fn process_response(&mut self, meta_result: Result<FetchMetadata, NetworkError>) {
        let (metadata, error) = match meta_result {
            Ok(meta) => (
                Some(match meta {
                    FetchMetadata::Unfiltered(m) => m,
                    FetchMetadata::Filtered { unsafe_, .. } => unsafe_,
                }),
                None,
            ),
            Err(error) => (
                // Check variant without moving
                match &error {
                    NetworkError::SslValidation(..) |
                    NetworkError::Internal(..) |
                    NetworkError::Crash(..) => {
                        let mut meta = Metadata::default(self.url.clone());
                        let mime: Option<Mime> = "text/html".parse().ok();
                        meta.set_content_type(mime.as_ref());
                        Some(meta)
                    },
                    _ => None,
                },
                Some(error),
            ),
        };
        let content_type: Option<Mime> = metadata
            .clone()
            .and_then(|meta| meta.content_type)
            .map(Serde::into_inner)
            .map(Into::into);

        // https://www.w3.org/TR/CSP/#initialize-document-csp
        // TODO: Implement step 1 (local scheme special case)
        let csp_list = metadata.as_ref().and_then(|m| {
            let h = m.headers.as_ref()?;
            let mut csp = h.get_all("content-security-policy").iter();
            // This silently ignores the CSP if it contains invalid Unicode.
            // We should probably report an error somewhere.
            let c = csp.next().and_then(|c| c.to_str().ok())?;
            let mut csp_list = CspList::parse(
                c,
                csp::PolicySource::Header,
                csp::PolicyDisposition::Enforce,
            );
            for c in csp {
                let c = c.to_str().ok()?;
                csp_list.append(CspList::parse(
                    c,
                    csp::PolicySource::Header,
                    csp::PolicyDisposition::Enforce,
                ));
            }
            Some(csp_list)
        });

        let parser = match ScriptThread::page_headers_available(&self.id, metadata) {
            Some(parser) => parser,
            None => return,
        };
        if parser.aborted.get() {
            return;
        }

        let _realm = enter_realm(&*parser.document);

        parser.document.set_csp_list(csp_list);
        self.parser = Some(Trusted::new(&*parser));
        self.submit_resource_timing();

        let content_type = match content_type {
            Some(ref content_type) => content_type,
            None => {
                // No content-type header.
                // Merge with #4212 when fixed.
                return;
            },
        };

        match (
            content_type.type_(),
            content_type.subtype(),
            content_type.suffix(),
        ) {
            (mime::IMAGE, _, _) => {
                self.is_synthesized_document = true;
                let page = "<html><body></body></html>".into();
                parser.push_string_input_chunk(page);
                parser.parse_sync();

                let doc = &parser.document;
                let doc_body = DomRoot::upcast::<Node>(doc.GetBody().unwrap());
                let img = HTMLImageElement::new(local_name!("img"), None, doc, None);
                img.SetSrc(USVString(self.url.to_string()));
                doc_body
                    .AppendChild(&DomRoot::upcast::<Node>(img))
                    .expect("Appending failed");
            },
            (mime::TEXT, mime::PLAIN, _) => {
                // https://html.spec.whatwg.org/multipage/#read-text
                let page = "<pre>\n".into();
                parser.push_string_input_chunk(page);
                parser.parse_sync();
                parser.tokenizer.borrow_mut().set_plaintext_state();
            },
            (mime::TEXT, mime::HTML, _) => match error {
                Some(NetworkError::SslValidation(reason, bytes)) => {
                    self.is_synthesized_document = true;
                    let page = resources::read_string(Resource::BadCertHTML);
                    let page = page.replace("${reason}", &reason);
                    let encoded_bytes = general_purpose::STANDARD_NO_PAD.encode(&bytes);
                    let page = page.replace("${bytes}", encoded_bytes.as_str());
                    let page =
                        page.replace("${secret}", &net_traits::PRIVILEGED_SECRET.to_string());
                    parser.push_string_input_chunk(page);
                    parser.parse_sync();
                },
                Some(NetworkError::Internal(reason)) => {
                    self.is_synthesized_document = true;
                    let page = resources::read_string(Resource::NetErrorHTML);
                    let page = page.replace("${reason}", &reason);
                    parser.push_string_input_chunk(page);
                    parser.parse_sync();
                },
                Some(NetworkError::Crash(details)) => {
                    self.is_synthesized_document = true;
                    let page = resources::read_string(Resource::CrashHTML);
                    let page = page.replace("${details}", &details);
                    parser.push_string_input_chunk(page);
                    parser.parse_sync();
                },
                Some(_) => {},
                None => {},
            },
            (mime::TEXT, mime::XML, _) |
            (mime::APPLICATION, mime::XML, _) |
            (mime::APPLICATION, mime::JSON, _) => {},
            (mime::APPLICATION, subtype, Some(mime::XML)) if subtype == "xhtml" => {},
            (mime_type, subtype, _) => {
                // Show warning page for unknown mime types.
                let page = format!(
                    "<html><body><p>Unknown content type ({}/{}).</p></body></html>",
                    mime_type.as_str(),
                    subtype.as_str()
                );
                self.is_synthesized_document = true;
                parser.push_string_input_chunk(page);
                parser.parse_sync();
            },
        }
    }

    fn process_response_chunk(&mut self, payload: Vec<u8>) {
        if self.is_synthesized_document {
            return;
        }
        let parser = match self.parser.as_ref() {
            Some(parser) => parser.root(),
            None => return,
        };
        if parser.aborted.get() {
            return;
        }
        let _realm = enter_realm(&*parser);
        parser.parse_bytes_chunk(payload);
    }

    // This method is called via script_thread::handle_fetch_eof, so we must call
    // submit_resource_timing in this function
    // Resource listeners are called via net_traits::Action::process, which handles submission for them
    fn process_response_eof(&mut self, status: Result<ResourceFetchTiming, NetworkError>) {
        let parser = match self.parser.as_ref() {
            Some(parser) => parser.root(),
            None => return,
        };
        if parser.aborted.get() {
            return;
        }

        let _realm = enter_realm(&*parser);

        match status {
            // are we throwing this away or can we use it?
            Ok(_) => (),
            // TODO(Savago): we should send a notification to callers #5463.
            Err(err) => debug!("Failed to load page URL {}, error: {:?}", self.url, err),
        }

        parser
            .document
            .set_redirect_count(self.resource_timing.redirect_count);

        parser.last_chunk_received.set(true);
        if !parser.suspended.get() {
            parser.parse_sync();
        }

        //TODO only update if this is the current document resource
        if let Some(pushed_index) = self.pushed_entry_index {
            let document = &parser.document;
            let performance_entry =
                PerformanceNavigationTiming::new(&document.global(), 0, 0, &document);
            document
                .global()
                .performance()
                .update_entry(pushed_index, performance_entry.upcast::<PerformanceEntry>());
        }
    }

    fn resource_timing_mut(&mut self) -> &mut ResourceFetchTiming {
        &mut self.resource_timing
    }

    fn resource_timing(&self) -> &ResourceFetchTiming {
        &self.resource_timing
    }

    // store a PerformanceNavigationTiming entry in the globalscope's Performance buffer
    fn submit_resource_timing(&mut self) {
        let parser = match self.parser.as_ref() {
            Some(parser) => parser.root(),
            None => return,
        };
        if parser.aborted.get() {
            return;
        }

        let document = &parser.document;

        //TODO nav_start and nav_start_precise
        let performance_entry =
            PerformanceNavigationTiming::new(&document.global(), 0, 0, &document);
        self.pushed_entry_index = document
            .global()
            .performance()
            .queue_entry(performance_entry.upcast::<PerformanceEntry>());
    }
}

impl PreInvoke for ParserContext {}

pub struct FragmentContext<'a> {
    pub context_elem: &'a Node,
    pub form_elem: Option<&'a Node>,
}

#[allow(crown::unrooted_must_root)]
fn insert(
    parent: &Node,
    reference_child: Option<&Node>,
    child: NodeOrText<Dom<Node>>,
    parsing_algorithm: ParsingAlgorithm,
) {
    match child {
        NodeOrText::AppendNode(n) => {
            // https://html.spec.whatwg.org/multipage/#insert-a-foreign-element
            // applies if this is an element; if not, it may be
            // https://html.spec.whatwg.org/multipage/#insert-a-comment
            let element_in_non_fragment =
                parsing_algorithm != ParsingAlgorithm::Fragment && n.is::<Element>();
            if element_in_non_fragment {
                ScriptThread::push_new_element_queue();
            }
            parent.InsertBefore(&n, reference_child).unwrap();
            if element_in_non_fragment {
                ScriptThread::pop_current_element_queue();
            }
        },
        NodeOrText::AppendText(t) => {
            // https://html.spec.whatwg.org/multipage/#insert-a-character
            let text = reference_child
                .and_then(Node::GetPreviousSibling)
                .or_else(|| parent.GetLastChild())
                .and_then(DomRoot::downcast::<Text>);

            if let Some(text) = text {
                text.upcast::<CharacterData>().append_data(&t);
            } else {
                let text = Text::new(String::from(t).into(), &parent.owner_doc());
                parent.InsertBefore(text.upcast(), reference_child).unwrap();
            }
        },
    }
}

#[derive(JSTraceable, MallocSizeOf)]
#[crown::unrooted_must_root_lint::must_root]
pub struct Sink {
    #[no_trace]
    base_url: ServoUrl,
    document: Dom<Document>,
    current_line: u64,
    script: MutNullableDom<HTMLScriptElement>,
    parsing_algorithm: ParsingAlgorithm,
}

impl Sink {
    fn same_tree(&self, x: &Dom<Node>, y: &Dom<Node>) -> bool {
        let x = x.downcast::<Element>().expect("Element node expected");
        let y = y.downcast::<Element>().expect("Element node expected");

        x.is_in_same_home_subtree(y)
    }

    fn has_parent_node(&self, node: &Dom<Node>) -> bool {
        node.GetParentNode().is_some()
    }
}

#[allow(crown::unrooted_must_root)] // FIXME: really?
impl TreeSink for Sink {
    type Output = Self;
    fn finish(self) -> Self {
        self
    }

    type Handle = Dom<Node>;

    fn get_document(&mut self) -> Dom<Node> {
        Dom::from_ref(self.document.upcast())
    }

    fn get_template_contents(&mut self, target: &Dom<Node>) -> Dom<Node> {
        let template = target
            .downcast::<HTMLTemplateElement>()
            .expect("tried to get template contents of non-HTMLTemplateElement in HTML parsing");
        Dom::from_ref(template.Content().upcast())
    }

    fn same_node(&self, x: &Dom<Node>, y: &Dom<Node>) -> bool {
        x == y
    }

    fn elem_name<'a>(&self, target: &'a Dom<Node>) -> ExpandedName<'a> {
        let elem = target
            .downcast::<Element>()
            .expect("tried to get name of non-Element in HTML parsing");
        ExpandedName {
            ns: elem.namespace(),
            local: elem.local_name(),
        }
    }

    fn create_element(
        &mut self,
        name: QualName,
        attrs: Vec<Attribute>,
        _flags: ElementFlags,
    ) -> Dom<Node> {
        let attrs = attrs
            .into_iter()
            .map(|attr| ElementAttribute::new(attr.name, DOMString::from(String::from(attr.value))))
            .collect();
        let element = create_element_for_token(
            name,
            attrs,
            &*self.document,
            ElementCreator::ParserCreated(self.current_line),
            self.parsing_algorithm,
        );
        Dom::from_ref(element.upcast())
    }

    fn create_comment(&mut self, text: StrTendril) -> Dom<Node> {
        let comment = Comment::new(DOMString::from(String::from(text)), &*self.document, None);
        Dom::from_ref(comment.upcast())
    }

    fn create_pi(&mut self, target: StrTendril, data: StrTendril) -> Dom<Node> {
        let doc = &*self.document;
        let pi = ProcessingInstruction::new(
            DOMString::from(String::from(target)),
            DOMString::from(String::from(data)),
            doc,
        );
        Dom::from_ref(pi.upcast())
    }

    fn associate_with_form(
        &mut self,
        target: &Dom<Node>,
        form: &Dom<Node>,
        nodes: (&Dom<Node>, Option<&Dom<Node>>),
    ) {
        let (element, prev_element) = nodes;
        let tree_node = prev_element.map_or(element, |prev| {
            if self.has_parent_node(element) {
                element
            } else {
                prev
            }
        });
        if !self.same_tree(tree_node, form) {
            return;
        }

        let node = target;
        let form = DomRoot::downcast::<HTMLFormElement>(DomRoot::from_ref(&**form))
            .expect("Owner must be a form element");

        let elem = node.downcast::<Element>();
        let control = elem.and_then(|e| e.as_maybe_form_control());

        if let Some(control) = control {
            control.set_form_owner_from_parser(&form);
        }
    }

    fn append_before_sibling(&mut self, sibling: &Dom<Node>, new_node: NodeOrText<Dom<Node>>) {
        let parent = sibling
            .GetParentNode()
            .expect("append_before_sibling called on node without parent");

        insert(&parent, Some(&*sibling), new_node, self.parsing_algorithm);
    }

    fn parse_error(&mut self, msg: Cow<'static, str>) {
        debug!("Parse error: {}", msg);
    }

    fn set_quirks_mode(&mut self, mode: QuirksMode) {
        let mode = match mode {
            QuirksMode::Quirks => ServoQuirksMode::Quirks,
            QuirksMode::LimitedQuirks => ServoQuirksMode::LimitedQuirks,
            QuirksMode::NoQuirks => ServoQuirksMode::NoQuirks,
        };
        self.document.set_quirks_mode(mode);
    }

    fn append(&mut self, parent: &Dom<Node>, child: NodeOrText<Dom<Node>>) {
        insert(&parent, None, child, self.parsing_algorithm);
    }

    fn append_based_on_parent_node(
        &mut self,
        elem: &Dom<Node>,
        prev_elem: &Dom<Node>,
        child: NodeOrText<Dom<Node>>,
    ) {
        if self.has_parent_node(elem) {
            self.append_before_sibling(elem, child);
        } else {
            self.append(prev_elem, child);
        }
    }

    fn append_doctype_to_document(
        &mut self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        let doc = &*self.document;
        let doctype = DocumentType::new(
            DOMString::from(String::from(name)),
            Some(DOMString::from(String::from(public_id))),
            Some(DOMString::from(String::from(system_id))),
            doc,
        );
        doc.upcast::<Node>()
            .AppendChild(doctype.upcast())
            .expect("Appending failed");
    }

    fn add_attrs_if_missing(&mut self, target: &Dom<Node>, attrs: Vec<Attribute>) {
        let elem = target
            .downcast::<Element>()
            .expect("tried to set attrs on non-Element in HTML parsing");
        for attr in attrs {
            elem.set_attribute_from_parser(
                attr.name,
                DOMString::from(String::from(attr.value)),
                None,
            );
        }
    }

    fn remove_from_parent(&mut self, target: &Dom<Node>) {
        if let Some(ref parent) = target.GetParentNode() {
            parent.RemoveChild(&*target).unwrap();
        }
    }

    fn mark_script_already_started(&mut self, node: &Dom<Node>) {
        let script = node.downcast::<HTMLScriptElement>();
        script.map(|script| script.set_already_started(true));
    }

    fn complete_script(&mut self, node: &Dom<Node>) -> NextParserState {
        if let Some(script) = node.downcast() {
            self.script.set(Some(script));
            NextParserState::Suspend
        } else {
            NextParserState::Continue
        }
    }

    fn reparent_children(&mut self, node: &Dom<Node>, new_parent: &Dom<Node>) {
        while let Some(ref child) = node.GetFirstChild() {
            new_parent.AppendChild(&child).unwrap();
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#html-integration-point>
    /// Specifically, the <annotation-xml> cases.
    fn is_mathml_annotation_xml_integration_point(&self, handle: &Dom<Node>) -> bool {
        let elem = handle.downcast::<Element>().unwrap();
        elem.get_attribute(&ns!(), &local_name!("encoding"))
            .map_or(false, |attr| {
                attr.value().eq_ignore_ascii_case("text/html") ||
                    attr.value().eq_ignore_ascii_case("application/xhtml+xml")
            })
    }

    fn set_current_line(&mut self, line_number: u64) {
        self.current_line = line_number;
    }

    fn pop(&mut self, node: &Dom<Node>) {
        let node = DomRoot::from_ref(&**node);
        vtable_for(&node).pop();
    }
}

/// <https://html.spec.whatwg.org/multipage/#create-an-element-for-the-token>
fn create_element_for_token(
    name: QualName,
    attrs: Vec<ElementAttribute>,
    document: &Document,
    creator: ElementCreator,
    parsing_algorithm: ParsingAlgorithm,
) -> DomRoot<Element> {
    // Step 3.
    let is = attrs
        .iter()
        .find(|attr| attr.name.local.eq_str_ignore_ascii_case("is"))
        .map(|attr| LocalName::from(&*attr.value));

    // Step 4.
    let definition = document.lookup_custom_element_definition(&name.ns, &name.local, is.as_ref());

    // Step 5.
    let will_execute_script =
        definition.is_some() && parsing_algorithm != ParsingAlgorithm::Fragment;

    // Step 6.
    if will_execute_script {
        // Step 6.1.
        document.increment_throw_on_dynamic_markup_insertion_counter();
        // Step 6.2
        if is_execution_stack_empty() {
            document
                .window()
                .upcast::<GlobalScope>()
                .perform_a_microtask_checkpoint();
        }
        // Step 6.3
        ScriptThread::push_new_element_queue()
    }

    // Step 7.
    let creation_mode = if will_execute_script {
        CustomElementCreationMode::Synchronous
    } else {
        CustomElementCreationMode::Asynchronous
    };

    let element = Element::create(name, is, document, creator, creation_mode, None);

    // https://html.spec.whatwg.org/multipage#the-input-element:value-sanitization-algorithm-3
    // says to invoke sanitization "when an input element is first created";
    // however, since sanitization requires content attributes to function,
    // it can't mean that literally.
    // Indeed, to make sanitization work correctly, we need to _not_ sanitize
    // until after all content attributes have been added

    let maybe_input = element.downcast::<HTMLInputElement>();
    if let Some(input) = maybe_input {
        input.disable_sanitization();
    }

    // Step 8
    for attr in attrs {
        element.set_attribute_from_parser(attr.name, attr.value, None);
    }

    // _now_ we can sanitize (and we sanitize now even if the "value"
    // attribute isn't present!)
    if let Some(input) = maybe_input {
        input.enable_sanitization();
    }

    // Step 9.
    if will_execute_script {
        // Steps 9.1 - 9.2.
        ScriptThread::pop_current_element_queue();
        // Step 9.3.
        document.decrement_throw_on_dynamic_markup_insertion_counter();
    }

    // TODO: Step 10.
    // TODO: Step 11.

    // Step 12 is handled in `associate_with_form`.

    // Step 13.
    element
}

#[derive(JSTraceable, MallocSizeOf)]
struct NetworkDecoder {
    #[ignore_malloc_size_of = "Defined in tendril"]
    #[custom_trace]
    decoder: LossyDecoder<NetworkSink>,
}

impl NetworkDecoder {
    fn new(encoding: &'static Encoding) -> Self {
        Self {
            decoder: LossyDecoder::new_encoding_rs(encoding, Default::default()),
        }
    }

    fn decode(&mut self, chunk: Vec<u8>) -> StrTendril {
        self.decoder.process(ByteTendril::from(&*chunk));
        mem::replace(
            &mut self.decoder.inner_sink_mut().output,
            Default::default(),
        )
    }

    fn finish(self) -> StrTendril {
        self.decoder.finish()
    }
}

#[derive(Default, JSTraceable)]
struct NetworkSink {
    #[no_trace]
    output: StrTendril,
}

impl TendrilSink<UTF8> for NetworkSink {
    type Output = StrTendril;

    fn process(&mut self, t: StrTendril) {
        if self.output.is_empty() {
            self.output = t;
        } else {
            self.output.push_tendril(&t);
        }
    }

    fn error(&mut self, _desc: Cow<'static, str>) {}

    fn finish(self) -> Self::Output {
        self.output
    }
}
