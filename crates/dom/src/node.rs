//! Node representation: intrusive sibling links in the arena plus per-kind
//! payloads (design doc §5.2).

use bitflags::bitflags;
use html5ever::interface::QuirksMode;
use html5ever::tendril::StrTendril;
use html5ever::{Attribute, LocalName, Namespace, QualName, local_name, ns};
use oxidepage_base::NodeId;
use servo_arc::Arc as ServoArc;
use smallvec::SmallVec;
use style::Atom;
use style::context::QuirksMode as StyleQuirksMode;
use style::properties::parse_style_attribute;
use style::shared_lock::SharedRwLock;
use style::stylesheets::{CssRuleType, UrlExtraData};

use crate::custom_element::CustomElementState;
use crate::form::FormState;
use crate::shadow::ShadowMode;
use crate::stylo_data::StyloElementState;
use crate::tree::make_url_extra_data;

bitflags! {
    /// Per-node state and dirty bits.
    ///
    /// Dirty bits are set by the single mutation code path (`DomTree`
    /// internals); the style system (Phase 4) and layout (Phase 5) consume
    /// and clear them.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
    pub struct NodeFlags: u8 {
        /// Node is in the document tree (spec "connected").
        const IS_CONNECTED = 1 << 0;
        /// Computed style of this node is out of date.
        const STYLE_DIRTY = 1 << 1;
        /// Layout of this node is out of date.
        const LAYOUT_DIRTY = 1 << 2;
        /// Paint output of this node is out of date.
        const PAINT_DIRTY = 1 << 3;
        /// Some descendant carries a dirty bit (ancestor-chain propagation).
        const HAS_DIRTY_DESCENDANT = 1 << 4;
    }
}

/// A node in the arena: intrusive tree links + payload.
pub struct Node {
    pub(crate) parent: Option<NodeId>,
    pub(crate) first_child: Option<NodeId>,
    pub(crate) last_child: Option<NodeId>,
    pub(crate) prev_sibling: Option<NodeId>,
    pub(crate) next_sibling: Option<NodeId>,
    pub(crate) flags: NodeFlags,
    pub(crate) data: NodeData,
    /// The node document (spec "node document"), i.e. `ownerDocument`.
    ///
    /// `None` **iff** this node *is* a Document — the spec's `ownerDocument`
    /// is null exactly there. Making the invariant biconditional (rather than
    /// "None means the page document") keeps a missing owner a bug instead of
    /// a silent default. `NodeId`'s generation is a `NonZeroU32`, so the
    /// `Option` is niche-packed and this field costs 8 bytes, not 12.
    pub(crate) owner: Option<NodeId>,
}

impl Node {
    /// A node owned by no document — a Document itself, or a node a test
    /// builds outside any tree.
    #[must_use]
    pub fn new(data: NodeData) -> Self {
        Self {
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            flags: NodeFlags::empty(),
            data,
            owner: None,
        }
    }

    /// A node whose node document is `owner`.
    #[must_use]
    pub fn new_in(data: NodeData, owner: NodeId) -> Self {
        Self {
            owner: Some(owner),
            ..Self::new(data)
        }
    }

    /// The node document (`ownerDocument`); `None` iff this node is a Document.
    #[must_use]
    pub fn owner(&self) -> Option<NodeId> {
        self.owner
    }

    #[must_use]
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    #[must_use]
    pub fn first_child(&self) -> Option<NodeId> {
        self.first_child
    }

    #[must_use]
    pub fn last_child(&self) -> Option<NodeId> {
        self.last_child
    }

    #[must_use]
    pub fn prev_sibling(&self) -> Option<NodeId> {
        self.prev_sibling
    }

    #[must_use]
    pub fn next_sibling(&self) -> Option<NodeId> {
        self.next_sibling
    }

    #[must_use]
    pub fn flags(&self) -> NodeFlags {
        self.flags
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.flags.contains(NodeFlags::IS_CONNECTED)
    }

    #[must_use]
    pub fn data(&self) -> &NodeData {
        &self.data
    }

    #[must_use]
    pub fn as_element(&self) -> Option<&ElementData> {
        match &self.data {
            NodeData::Element(el) => Some(el),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_element_mut(&mut self) -> Option<&mut ElementData> {
        match &mut self.data {
            NodeData::Element(el) => Some(el),
            _ => None,
        }
    }

    /// Text content for character-data nodes (Text, CDATASection, Comment, PI).
    #[must_use]
    pub fn character_data(&self) -> Option<&StrTendril> {
        match &self.data {
            NodeData::Text(t) | NodeData::CdataSection(t) | NodeData::Comment(t) => Some(t),
            NodeData::ProcessingInstruction { data, .. } => Some(data),
            _ => None,
        }
    }

    /// Whether this node is a Text node in the spec's sense — which includes
    /// `CDATASection`, since `interface CDATASection : Text`. Every rule that
    /// says "Text" (hierarchy validity, `:empty`, whitespace stripping, style
    /// and layout) means *this*, not the `Text` variant alone.
    #[must_use]
    pub fn is_text(&self) -> bool {
        is_text_kind(self.data.kind())
    }

    /// Whether this node is an HTML `<style>` element.
    #[must_use]
    pub fn is_style_element(&self) -> bool {
        self.as_element()
            .is_some_and(|el| el.is_html_element() && el.name.local == local_name!("style"))
    }

    /// Whether this node is a `<link rel="stylesheet" href="…">` element (the
    /// `rel` token is matched ASCII-case-insensitively).
    #[must_use]
    pub fn is_stylesheet_link(&self) -> bool {
        let Some(el) = self.as_element() else {
            return false;
        };
        if !el.is_html_element() || el.name.local != local_name!("link") {
            return false;
        }
        let has_stylesheet_rel = el.attr(&attr_name(local_name!("rel"))).is_some_and(|rel| {
            rel.split_ascii_whitespace()
                .any(|tok| tok.eq_ignore_ascii_case("stylesheet"))
        });
        has_stylesheet_rel && el.attr(&attr_name(local_name!("href"))).is_some()
    }

    /// Whether this node is an HTML `<img>` element with a non-empty `src`.
    #[must_use]
    pub fn is_image_element(&self) -> bool {
        self.as_element().is_some_and(|el| {
            el.is_html_element()
                && el.name.local == local_name!("img")
                && el
                    .attr(&attr_name(local_name!("src")))
                    .is_some_and(|src| !src.is_empty())
        })
    }
}

/// Per-kind node payload.
pub enum NodeData {
    Document(DocumentData),
    /// A `DocumentFragment`; also used for `<template>` contents and shadow
    /// roots, in which case `host` points back at the owning element.
    /// `shadow` is `Some(mode)` exactly for shadow roots (template contents
    /// and plain fragments carry `None`).
    DocumentFragment {
        host: Option<NodeId>,
        shadow: Option<ShadowMode>,
    },
    Doctype {
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    },
    Element(Box<ElementData>),
    Text(StrTendril),
    /// `CDATASection`. A Text node for every spec rule — see [`Node::is_text`].
    CdataSection(StrTendril),
    Comment(StrTendril),
    ProcessingInstruction {
        target: StrTendril,
        data: StrTendril,
    },
}

impl NodeData {
    /// Spec `nodeType`-style kind, for constraint checks and debugging.
    #[must_use]
    pub fn kind(&self) -> NodeKind {
        match self {
            NodeData::Document(_) => NodeKind::Document,
            NodeData::DocumentFragment { .. } => NodeKind::DocumentFragment,
            NodeData::Doctype { .. } => NodeKind::Doctype,
            NodeData::Element(_) => NodeKind::Element,
            NodeData::Text(_) => NodeKind::Text,
            NodeData::CdataSection(_) => NodeKind::CdataSection,
            NodeData::Comment(_) => NodeKind::Comment,
            NodeData::ProcessingInstruction { .. } => NodeKind::ProcessingInstruction,
        }
    }
}

/// Discriminant-only view of [`NodeData`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Document,
    DocumentFragment,
    Doctype,
    Element,
    Text,
    CdataSection,
    Comment,
    ProcessingInstruction,
}

impl NodeKind {
    /// The spec's `Node.nodeType` constant for this kind.
    ///
    /// Lives here rather than in the bindings layer because it is a DOM
    /// concept: the protocol surface (`DOM.describeNode`) reports it without
    /// entering JS, and a second copy of the mapping would drift.
    #[must_use]
    pub fn node_type(self) -> u16 {
        match self {
            NodeKind::Element => 1,
            NodeKind::Text => 3,
            NodeKind::CdataSection => 4,
            NodeKind::ProcessingInstruction => 7,
            NodeKind::Comment => 8,
            NodeKind::Document => 9,
            NodeKind::Doctype => 10,
            NodeKind::DocumentFragment => 11,
        }
    }
}

/// The spec's `Node.nodeName` for `id`.
///
/// Element names are *qualified* (a prefixed element reports `x:b`, not `b`)
/// and ASCII-upper-cased in the HTML namespace. Same reasoning as
/// [`NodeKind::node_type`]: both the JS getter and the protocol's node
/// description need it, and only one of them can enter JS.
///
/// # Panics
///
/// Panics if `id` is stale — it goes through [`DomTree::node`].
#[must_use]
pub fn node_name(dom: &crate::tree::DomTree, id: NodeId) -> String {
    match dom.node(id).data() {
        NodeData::Element(el) => {
            let name = qualified_name(&el.name);
            if el.is_html_element() {
                name.to_ascii_uppercase()
            } else {
                name
            }
        }
        NodeData::Text(_) => "#text".to_owned(),
        NodeData::CdataSection(_) => "#cdata-section".to_owned(),
        NodeData::Comment(_) => "#comment".to_owned(),
        NodeData::Document(_) => "#document".to_owned(),
        NodeData::DocumentFragment { .. } => "#document-fragment".to_owned(),
        NodeData::Doctype { name, .. } => name.to_string(),
        NodeData::ProcessingInstruction { target, .. } => target.to_string(),
    }
}

/// `prefix:local`, or bare `local` when there is no prefix.
#[must_use]
pub fn qualified_name(name: &QualName) -> String {
    match &name.prefix {
        Some(prefix) => format!("{prefix}:{}", name.local),
        None => name.local.to_string(),
    }
}

/// Whether `kind` is a Text node in the spec's sense (`CDATASection : Text`).
///
/// Exists so that a `match` on [`NodeKind`] cannot quietly forget the CDATA
/// arm: the hierarchy checks in [`DomTree`](crate::DomTree) route through this
/// rather than naming `NodeKind::Text` directly, because "a Document must not
/// have a Text child" forbids a CDATASection child too.
#[must_use]
pub fn is_text_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Text | NodeKind::CdataSection)
}

/// Whether a document is an HTML document or an XML document (spec: the
/// document's "type"). Drives `createElement` name-lowercasing, the namespace
/// it assigns, and whether `createCDATASection` is allowed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DocumentKind {
    Html,
    Xml,
}

/// Payload of a document node.
pub struct DocumentData {
    pub quirks_mode: QuirksMode,
    /// The document's URL (spec: every document has one).
    ///
    /// Private, and paired with [`Self::url_extra`], because the two must not
    /// drift: stylo resolves every relative URL in a parsed style attribute or
    /// stylesheet against `url_extra`, so a document whose URL moved without it
    /// would resolve against the address it used to have. [`Self::set_url`] is
    /// the only writer.
    url: String,
    /// `url` as stylo's URL data. Per document since ADR-0035 D1 — a nested
    /// browsing context resolves against *its* address, not the top-level one.
    url_extra: UrlExtraData,
    /// The document's type. `new Document()` and `createDocument()` are XML.
    pub kind: DocumentKind,
    /// The document's content type, verbatim (`document.contentType`).
    pub content_type: String,
    /// Whether the document exposes the `XMLDocument` interface rather than
    /// `Document`. Per spec this is *not* implied by [`DocumentKind::Xml`]:
    /// `createDocument()` returns an `XMLDocument`, but `new Document()`
    /// returns a plain `Document` even though both are XML documents. The
    /// wrapper's prototype is chosen from `NodeData` alone, so the bit has to
    /// live here.
    pub xml_document_interface: bool,
}

impl DocumentData {
    /// A document created by parsing HTML — the page document, and the result
    /// of `createHTMLDocument()` / `DOMParser.parseFromString(…, "text/html")`.
    #[must_use]
    pub fn html(url: String) -> Self {
        let mut data = Self::default();
        data.set_url(url);
        data
    }

    /// An XML document: `new Document()`, `createDocument()`, and the XML
    /// content types of `DOMParser`.
    #[must_use]
    pub fn xml(url: String, content_type: String, xml_document_interface: bool) -> Self {
        Self {
            quirks_mode: QuirksMode::NoQuirks,
            url_extra: make_url_extra_data(&url),
            url,
            kind: DocumentKind::Xml,
            content_type,
            xml_document_interface,
        }
    }

    /// The document's URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The document's URL as stylo's URL data, for relative-URL resolution.
    #[must_use]
    pub fn url_extra(&self) -> &UrlExtraData {
        &self.url_extra
    }

    /// Moves the document's URL, keeping the stylo URL data in step.
    pub fn set_url(&mut self, url: String) {
        self.url_extra = make_url_extra_data(&url);
        self.url = url;
    }

    #[must_use]
    pub fn is_html(&self) -> bool {
        self.kind == DocumentKind::Html
    }
}

impl Default for DocumentData {
    /// The page document: an HTML document at `about:blank`.
    fn default() -> Self {
        Self {
            quirks_mode: QuirksMode::NoQuirks,
            url: "about:blank".to_owned(),
            url_extra: make_url_extra_data("about:blank"),
            kind: DocumentKind::Html,
            content_type: "text/html".to_owned(),
            xml_document_interface: false,
        }
    }
}

/// Payload of an element node.
pub struct ElementData {
    /// Interned qualified name (namespace + local name).
    pub name: QualName,
    pub(crate) attrs: Vec<Attribute>,
    /// Cache of the `id` attribute as a stylo atom, shared by `querySelector`
    /// and the cascade (Phase 2+).
    pub(crate) id: Option<Atom>,
    /// Cache of the `class` attribute tokens as stylo atoms.
    pub(crate) classes: SmallVec<[Atom; 4]>,
    /// Stylo cascade state (computed styles, parsed `style` attribute,
    /// pseudo-class state, invalidation bits). See [`StyloElementState`].
    pub stylo: StyloElementState,
    /// `<template>` contents fragment; a distinct tree, not a child.
    pub(crate) template_contents: Option<NodeId>,
    /// Shadow root fragment attached via `attachShadow`; a distinct tree
    /// that participates in connectedness, style, and layout (flat tree).
    pub(crate) shadow_root: Option<NodeId>,
    /// Set by the parser for `annotation-xml` HTML integration points.
    pub(crate) mathml_annotation_xml_integration_point: bool,
    /// HTML `<script>` "already started" flag (parser bookkeeping).
    pub(crate) script_already_started: bool,
    /// HTML script "force async" flag. DOM-created scripts start true;
    /// parser-created scripts keep the default false.
    pub(crate) script_force_async: bool,
    /// Custom-element lifecycle state (HTML "custom element state"). Definitions
    /// live in the bindings layer; the DOM tracks only per-element state and a
    /// reaction-intent queue on [`DomTree`](crate::DomTree).
    pub(crate) custom_state: CustomElementState,
    /// Form-control state that the content attributes do not describe: the
    /// dirty value/checkedness flags. Allocated only for the elements that have
    /// it — see [`FormState`](crate::form::FormState).
    pub(crate) form: Option<Box<FormState>>,
}

impl ElementData {
    #[must_use]
    pub fn new(name: QualName) -> Self {
        Self {
            name,
            attrs: Vec::new(),
            id: None,
            classes: SmallVec::new(),
            stylo: StyloElementState::default(),
            template_contents: None,
            shadow_root: None,
            mathml_annotation_xml_integration_point: false,
            script_already_started: false,
            script_force_async: false,
            custom_state: CustomElementState::default(),
            form: None,
        }
    }

    #[must_use]
    pub fn custom_state(&self) -> CustomElementState {
        self.custom_state
    }

    pub fn set_custom_state(&mut self, state: CustomElementState) {
        self.custom_state = state;
    }

    #[must_use]
    pub fn attrs(&self) -> &[Attribute] {
        &self.attrs
    }

    /// Value of the attribute with the given name, if present.
    #[must_use]
    pub fn attr(&self, name: &QualName) -> Option<&StrTendril> {
        self.attrs
            .iter()
            .find(|a| a.name == *name)
            .map(|a| &a.value)
    }

    #[must_use]
    pub fn id(&self) -> Option<&Atom> {
        self.id.as_ref()
    }

    #[must_use]
    pub fn classes(&self) -> &[Atom] {
        &self.classes
    }

    #[must_use]
    pub fn template_contents(&self) -> Option<NodeId> {
        self.template_contents
    }

    /// The shadow root attached to this element, regardless of mode.
    #[must_use]
    pub fn shadow_root(&self) -> Option<NodeId> {
        self.shadow_root
    }

    #[must_use]
    pub fn is_html_element(&self) -> bool {
        self.name.ns == ns!(html)
    }

    #[must_use]
    pub fn is_svg_element(&self) -> bool {
        self.name.ns == ns!(svg)
    }

    /// Recomputes the `id`/`class`/`style` caches from `attrs`.
    ///
    /// Kept in one place so every attribute mutation path shares it. The
    /// `style` attribute is reparsed into a locked declaration block (shared
    /// with the cascade and CSSOM); `lock`/`url` come from the owning
    /// [`DomTree`](crate::tree::DomTree).
    pub(crate) fn refresh_selector_caches(&mut self, lock: &SharedRwLock, url: &UrlExtraData) {
        self.id = None;
        self.classes.clear();
        let mut style_src: Option<String> = None;
        for attr in &self.attrs {
            if attr.name.ns != ns!() {
                continue;
            }
            if attr.name.local == local_name!("id") {
                self.id = Some(Atom::from(&*attr.value));
            } else if attr.name.local == local_name!("class") {
                self.classes = attr
                    .value
                    .split_ascii_whitespace()
                    .map(Atom::from)
                    .collect();
            } else if attr.name.local == local_name!("style") {
                style_src = Some(attr.value.to_string());
            }
        }
        self.stylo.style_attribute = style_src.map(|src| {
            let block = parse_style_attribute(
                &src,
                url,
                None,
                StyleQuirksMode::NoQuirks,
                CssRuleType::Style,
            );
            ServoArc::new(lock.wrap(block))
        });
    }
}

/// Helper for building HTML-namespace qualified names.
#[must_use]
pub fn html_name(local: LocalName) -> QualName {
    QualName::new(None, ns!(html), local)
}

/// Helper for building no-namespace attribute names.
#[must_use]
pub fn attr_name(local: LocalName) -> QualName {
    QualName::new(None, ns!(), local)
}

/// Helper for building a namespaced qualified name.
#[must_use]
pub fn qual_name(ns: Namespace, local: LocalName) -> QualName {
    QualName::new(None, ns, local)
}
