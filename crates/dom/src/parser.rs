//! Suspendable HTML parsing driver (design doc §5.2).
//!
//! The parser is driven at the tokenizer level rather than through
//! html5ever's `TendrilSink` so that `</script>` becomes a suspension point:
//! [`Parser::run`] returns [`ParseSignal::Script`] and the caller (the event
//! loop, in Phase 2+) executes the script — possibly mutating the DOM —
//! before resuming. Parser-time `document.write` can prepend input while the
//! tokenizer is suspended at that script.

use html5ever::buffer_queue::BufferQueue;
use html5ever::interface::TreeSink;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{Tokenizer, TokenizerOpts};
use html5ever::tree_builder::{TreeBuilder, TreeBuilderOpts};
use html5ever::{Attribute, QualName, TokenizerResult, interface};
use oxidepage_base::NodeId;

use crate::sink::{ParsedDocument, Sink};
use crate::tree::DomTree;

/// Parser configuration (subset of html5ever's options the engine exposes).
#[derive(Clone, Copy, Debug)]
pub struct ParseOptions {
    /// Affects `<noscript>` parsing; mirrors whether the page has JS.
    pub scripting_enabled: bool,
    /// Parsing an `<iframe srcdoc>` document (quirks-mode heuristics).
    pub iframe_srcdoc: bool,
    /// Report all spec parse errors (slower; used by conformance tests).
    pub exact_errors: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            scripting_enabled: true,
            iframe_srcdoc: false,
            exact_errors: false,
        }
    }
}

impl ParseOptions {
    fn to_html5ever(self) -> (TokenizerOpts, TreeBuilderOpts) {
        (
            TokenizerOpts {
                exact_errors: self.exact_errors,
                ..TokenizerOpts::default()
            },
            TreeBuilderOpts {
                exact_errors: self.exact_errors,
                scripting_enabled: self.scripting_enabled,
                iframe_srcdoc: self.iframe_srcdoc,
                ..TreeBuilderOpts::default()
            },
        )
    }
}

/// What the parser is waiting for after a [`Parser::run`] call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseSignal {
    /// Input exhausted; feed more input or call [`Parser::finish`].
    InputExhausted,
    /// A `<script>` element finished parsing. The caller should execute it
    /// (Phase 2+) and then call [`Parser::run`] again to resume.
    Script(NodeId),
}

/// A suspendable streaming HTML parser writing into a [`DomTree`] arena.
pub struct Parser {
    tokenizer: Tokenizer<TreeBuilder<NodeId, Sink>>,
    input: BufferQueue,
}

impl Parser {
    /// A parser for a full document.
    #[must_use]
    pub fn new_document(opts: ParseOptions) -> Self {
        let (tok_opts, tb_opts) = opts.to_html5ever();
        let tree_builder = TreeBuilder::new(Sink::default(), tb_opts);
        Self {
            tokenizer: Tokenizer::new(tree_builder, tok_opts),
            input: BufferQueue::default(),
        }
    }

    /// A parser for a full document writing into a tree shared with the
    /// embedder (scripts executing at suspension points see the same arena).
    /// End it with [`Parser::finish_shared`], not [`Parser::finish`].
    #[must_use]
    pub fn new_document_shared(
        tree: std::rc::Rc<std::cell::RefCell<DomTree>>,
        opts: ParseOptions,
    ) -> Self {
        let document = tree.borrow().document();
        Self::new_document_shared_at(tree, document, opts)
    }

    /// A parser for a full document writing into `document` — a Document node
    /// of the shared `tree`, not necessarily the page document. This is what
    /// makes `DOMParser` a real full-document parse rather than an
    /// `innerHTML` approximation.
    #[must_use]
    pub fn new_document_shared_at(
        tree: std::rc::Rc<std::cell::RefCell<DomTree>>,
        document: NodeId,
        opts: ParseOptions,
    ) -> Self {
        let (tok_opts, tb_opts) = opts.to_html5ever();
        let tree_builder = TreeBuilder::new(Sink::shared_at(tree, document), tb_opts);
        Self {
            tokenizer: Tokenizer::new(tree_builder, tok_opts),
            input: BufferQueue::default(),
        }
    }

    /// A parser for a fragment, per the HTML fragment parsing algorithm,
    /// with a fresh context element named `context`.
    ///
    /// Parsed content ends up as children of the `html` root element the
    /// algorithm appends to the document.
    #[must_use]
    pub fn new_fragment(
        context: QualName,
        context_attrs: Vec<Attribute>,
        opts: ParseOptions,
    ) -> Self {
        let (tok_opts, tb_opts) = opts.to_html5ever();
        let sink = Sink::default();
        let context_elem = interface::create_element(&sink, context, context_attrs);
        let tree_builder = TreeBuilder::new_for_fragment(sink, context_elem, None, tb_opts);
        let tok_opts = TokenizerOpts {
            initial_state: Some(
                tree_builder.tokenizer_state_for_context_elem(opts.scripting_enabled),
            ),
            ..tok_opts
        };
        Self {
            tokenizer: Tokenizer::new(tree_builder, tok_opts),
            input: BufferQueue::default(),
        }
    }

    /// Queues a chunk of input. Does not parse; call [`Parser::run`].
    pub fn push_input(&mut self, chunk: StrTendril) {
        self.input.push_back(chunk);
    }

    /// Queues parser-inserted markup before the unconsumed network/input
    /// stream, as required by practical parser-time `document.write`.
    pub fn push_front_input(&mut self, chunk: StrTendril) {
        self.input.push_front(chunk);
    }

    /// Parses queued input until it is exhausted or a script suspension
    /// point is reached.
    pub fn run(&mut self) -> ParseSignal {
        loop {
            match self.tokenizer.feed(&self.input) {
                TokenizerResult::Done => return ParseSignal::InputExhausted,
                TokenizerResult::Script(node) => return ParseSignal::Script(node),
                // Phase 1 decodes the whole byte stream up front (§ decode);
                // restart-with-new-encoding arrives with streaming network
                // loads in Phase 3.
                TokenizerResult::EncodingIndicator(_) => continue,
            }
        }
    }

    /// Signals end of input, drains remaining parsing, and returns the tree.
    ///
    /// Any pending script suspension points are passed through without
    /// execution (there is no JS in Phase 1).
    #[must_use]
    pub fn finish(mut self) -> ParsedDocument {
        loop {
            match self.run() {
                ParseSignal::InputExhausted => break,
                ParseSignal::Script(_) => continue,
            }
        }
        assert!(
            self.input.is_empty(),
            "parser finished with unconsumed input"
        );
        self.tokenizer.end();
        self.tokenizer.sink.sink.finish()
    }

    /// Ends parsing on a shared tree, returning the collected parse errors
    /// (the tree stays with the embedder). Pending script suspension points
    /// are passed through without execution.
    pub fn finish_shared(mut self) -> Vec<std::borrow::Cow<'static, str>> {
        loop {
            match self.run() {
                ParseSignal::InputExhausted => break,
                ParseSignal::Script(_) => continue,
            }
        }
        self.tokenizer.end();
        self.tokenizer.sink.sink.take_errors()
    }
}

/// Parses a complete UTF-8 document in one call, ignoring script suspension
/// points (scripts are parsed into the tree but not executed).
#[must_use]
pub fn parse_document(html: &str, opts: ParseOptions) -> ParsedDocument {
    let mut parser = Parser::new_document(opts);
    parser.push_input(html.into());
    parser.finish()
}

/// Parses a fragment in one call; see [`Parser::new_fragment`].
#[must_use]
pub fn parse_fragment(
    html: &str,
    context: QualName,
    context_attrs: Vec<Attribute>,
    opts: ParseOptions,
) -> ParsedDocument {
    let mut parser = Parser::new_fragment(context, context_attrs, opts);
    parser.push_input(html.into());
    parser.finish()
}

/// Implements `innerHTML =`-style fragment parsing into an existing tree:
/// parses `html` with `context` as the context element name and returns the
/// parsed nodes **moved** into `tree` as a detached document fragment owned by
/// `owner` (the context element's node document).
pub fn parse_fragment_into(
    tree: &mut DomTree,
    html: &str,
    context: QualName,
    opts: ParseOptions,
    owner: NodeId,
) -> NodeId {
    let parsed = parse_fragment(html, context, Vec::new(), opts);
    let source = parsed.tree;
    let fragment = tree.create_document_fragment_in(owner);
    let Some(root) = source.document_element() else {
        return fragment;
    };
    tree.graft_subtree_children(&source, root, fragment);
    fragment
}

/// Parses a complete document into `document`, an existing (second) Document
/// node of `tree` — the engine's real `DOMParser.parseFromString`.
///
/// Script suspension points are passed through without execution. Callers pass
/// `scripting_enabled: false`: a `DOMParser` document has no browsing context,
/// so its scripts never run and `<noscript>` must parse as if scripting were
/// off.
///
/// Safe to call from inside a script that the page parser is suspended on: the
/// nested parse runs to completion synchronously, executes no JS, and only
/// allocates arena slots — it never frees one, so the outer parser's
/// open-element stack cannot go stale underneath it.
pub fn parse_into_document(
    tree: &std::rc::Rc<std::cell::RefCell<DomTree>>,
    document: NodeId,
    html: &str,
    opts: ParseOptions,
) -> Vec<std::borrow::Cow<'static, str>> {
    let mut parser = Parser::new_document_shared_at(tree.clone(), document, opts);
    parser.push_input(html.into());
    parser.finish_shared()
}
