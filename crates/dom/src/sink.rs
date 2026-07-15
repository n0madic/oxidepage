//! html5ever `TreeSink` implemented directly over the [`DomTree`] arena:
//! parsing streams into the arena with no intermediate representation
//! (design doc §5.2).
//!
//! The sink wraps the tree in a `RefCell` because the parser drives it
//! through `&self`; the tree itself keeps a plain `&mut` API for the rest of
//! the engine. Sink mutations go through the same internal primitives as the
//! public spec algorithms, so parser-driven mutations set dirty bits and
//! queue observer records like any others.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use html5ever::interface::{ElemName, ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::StrTendril;
use html5ever::{Attribute, LocalName, Namespace, QualName};
use oxidepage_base::NodeId;

use crate::node::NodeData;
use crate::tree::DomTree;

/// The result of a parse: the tree plus collected parse errors.
pub struct ParsedDocument {
    pub tree: DomTree,
    pub errors: Vec<Cow<'static, str>>,
}

/// Owned element name returned to the tree builder; atoms are cheap clones.
pub struct OwnedElemName {
    ns: Namespace,
    local: LocalName,
}

impl std::fmt::Debug for OwnedElemName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.ns, self.local)
    }
}

impl ElemName for OwnedElemName {
    fn ns(&self) -> &Namespace {
        &self.ns
    }

    fn local_name(&self) -> &LocalName {
        &self.local
    }
}

/// html5ever tree sink over a [`DomTree`].
///
/// The tree is behind `Rc<RefCell<…>>` so an embedder can share it with the
/// JS bindings while parsing is suspended at a `</script>` (design doc §5.2:
/// parser and script mutate one arena). The sink never holds a borrow across
/// a suspension point.
pub struct Sink {
    tree: Rc<RefCell<DomTree>>,
    /// The document this parse builds into. Normally the page document, but
    /// `DOMParser` points it at a second Document — html5ever asks for the
    /// document handle exactly once (at `TreeBuilder::new`) and appends the
    /// doctype and `<html>` to whatever it gets back, so this one field is the
    /// whole of "parse a full document somewhere else".
    document: NodeId,
    errors: RefCell<Vec<Cow<'static, str>>>,
}

impl Default for Sink {
    fn default() -> Self {
        Self::new(DomTree::new())
    }
}

impl Sink {
    #[must_use]
    pub fn new(tree: DomTree) -> Self {
        Self::shared(Rc::new(RefCell::new(tree)))
    }

    /// A sink parsing into the page document of a tree shared with the
    /// embedder.
    #[must_use]
    pub fn shared(tree: Rc<RefCell<DomTree>>) -> Self {
        let document = tree.borrow().document();
        Self::shared_at(tree, document)
    }

    /// A sink parsing into `document`, which must be a Document node of `tree`.
    #[must_use]
    pub fn shared_at(tree: Rc<RefCell<DomTree>>, document: NodeId) -> Self {
        Self {
            tree,
            document,
            errors: RefCell::new(Vec::new()),
        }
    }

    /// Drains collected parse errors (shared-tree mode, where
    /// [`TreeSink::finish`] cannot be used).
    pub(crate) fn take_errors(&self) -> Vec<Cow<'static, str>> {
        std::mem::take(&mut self.errors.borrow_mut())
    }
}

impl TreeSink for Sink {
    type Handle = NodeId;
    type Output = ParsedDocument;
    type ElemName<'a> = OwnedElemName;

    fn finish(self) -> ParsedDocument {
        let errors = self.errors.into_inner();
        let tree = Rc::try_unwrap(self.tree)
            .unwrap_or_else(|_| panic!("finish() on a shared tree; use Parser::finish_shared"))
            .into_inner();
        ParsedDocument { tree, errors }
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
        self.errors.borrow_mut().push(msg);
    }

    fn get_document(&self) -> NodeId {
        self.document
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> OwnedElemName {
        let tree = self.tree.borrow();
        let el = tree
            .node(*target)
            .as_element()
            .expect("elem_name called on a non-element node");
        OwnedElemName {
            ns: el.name.ns.clone(),
            local: el.name.local.clone(),
        }
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        let mut tree = self.tree.borrow_mut();
        let id = tree.create_element_in(self.document, name, attrs);
        if flags.template {
            tree.create_template_contents(id);
        }
        if flags.mathml_annotation_xml_integration_point
            && let Some(el) = tree.node_mut_internal(id).as_element_mut()
        {
            el.mathml_annotation_xml_integration_point = true;
        }
        id
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.tree
            .borrow_mut()
            .create_comment_in(self.document, text)
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
        self.tree
            .borrow_mut()
            .create_processing_instruction_in(self.document, target, data)
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        let mut tree = self.tree.borrow_mut();
        match child {
            NodeOrText::AppendNode(node) => {
                tree.insert_internal(node, *parent, None, false);
            }
            NodeOrText::AppendText(text) => {
                // Merge with an existing trailing text node.
                if let Some(last) = tree.node(*parent).last_child()
                    && matches!(tree.node(last).data(), NodeData::Text(_))
                {
                    tree.append_to_text(last, &text);
                    return;
                }
                let text_node = tree.create_text_in(self.document, text);
                tree.insert_internal(text_node, *parent, None, false);
            }
        }
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let mut tree = self.tree.borrow_mut();
        let parent = tree
            .node(*sibling)
            .parent()
            .expect("append_before_sibling target must have a parent");
        match new_node {
            NodeOrText::AppendNode(node) => {
                tree.insert_internal(node, parent, Some(*sibling), false);
            }
            NodeOrText::AppendText(text) => {
                // Merge with the previous sibling if it is a text node.
                if let Some(prev) = tree.node(*sibling).prev_sibling()
                    && matches!(tree.node(prev).data(), NodeData::Text(_))
                {
                    tree.append_to_text(prev, &text);
                    return;
                }
                let text_node = tree.create_text_in(self.document, text);
                tree.insert_internal(text_node, parent, Some(*sibling), false);
            }
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let has_parent = self.tree.borrow().node(*element).parent().is_some();
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        let mut tree = self.tree.borrow_mut();
        let doctype = tree.create_doctype_in(self.document, name, public_id, system_id);
        tree.insert_internal(doctype, self.document, None, false);
    }

    fn mark_script_already_started(&self, node: &NodeId) {
        let mut tree = self.tree.borrow_mut();
        if let Some(el) = tree.node_mut_internal(*node).as_element_mut() {
            el.script_already_started = true;
        }
    }

    fn pop(&self, node: &NodeId) {
        // A `<style>` element being popped means its text content is complete,
        // so its stylesheet can now be built (design doc §10).
        self.tree.borrow_mut().note_style_element_closed(*node);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        self.tree
            .borrow()
            .node(*target)
            .as_element()
            .and_then(|el| el.template_contents())
            .expect("get_template_contents called on a non-template element")
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.tree
            .borrow_mut()
            .set_quirks_mode_of(self.document, mode);
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        self.tree.borrow_mut().add_attrs_if_missing(*target, attrs);
    }

    fn associate_with_form(
        &self,
        _target: &NodeId,
        _form: &NodeId,
        _nodes: (&NodeId, Option<&NodeId>),
    ) {
        // Form owners land with form-associated element support (Phase 2+).
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.tree.borrow_mut().remove_internal(*target, false);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut tree = self.tree.borrow_mut();
        let children: Vec<NodeId> = tree.children(*node).collect();
        for child in children {
            tree.remove_internal(child, false);
            tree.insert_internal(child, *new_parent, None, false);
        }
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &NodeId) -> bool {
        self.tree
            .borrow()
            .node(*handle)
            .as_element()
            .is_some_and(|el| el.mathml_annotation_xml_integration_point)
    }
}
