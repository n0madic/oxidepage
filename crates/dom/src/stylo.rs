//! Lets the arena DOM participate in stylo's cascade by implementing stylo's
//! tree traits (`TDocument`, `TNode`, `TElement`, …) over the [`NodeRef`]
//! handle (design doc §10, ADR-0005). Ported from `blitz-dom`'s `stylo.rs`,
//! adapted from its `&Node` handle to our `(&DomTree, NodeId)` handle.
//!
//! Stylo mutates element data through **shared** references during traversal;
//! the interior mutability that makes this sound lives in
//! [`StyloElementState`](crate::stylo_data::StyloElementState). This module is
//! `#![allow(unsafe_code)]` for the few `&self` mutation entry points stylo's
//! `TElement` requires; each is justified by stylo's exclusive per-node access.
#![allow(unsafe_code)]

use std::num::NonZeroU32;
use std::sync::atomic::Ordering;

use html5ever::{LocalName, Namespace, local_name};
use oxidepage_base::NodeId;
use selectors::matching::{ElementSelectorFlags, VisitedHandlingMode};
use selectors::sink::Push;
use servo_arc::{Arc as ServoArc, ArcBorrow};
use style::applicable_declarations::ApplicableDeclarationBlock;
use style::context::{QuirksMode, SharedStyleContext};
use style::data::{ElementDataMut, ElementDataRef};
use style::dom::{
    AttributeProvider, LayoutIterator, NodeInfo, OpaqueNode, TDocument, TElement, TNode,
    TShadowRoot,
};
use style::properties::{ComputedValues, PropertyDeclarationBlock};
use style::selector_parser::{AttrValue, Lang, PseudoElement, RestyleDamage};
use style::shared_lock::{Locked, SharedRwLock};
use style::stylesheets::scope_rule::ImplicitScopeRoot;
use style::values::AtomIdent;
use style::values::GenericAtomIdent;
use style_dom::ElementState;

use crate::node::NodeKind;
use crate::select::NodeRef;

// `opaque_node` packs a 32-bit `NodeId` index and 32-bit generation into a
// single `usize`, and `node_id_from_opaque` splits them back out at bit 32.
// That is only lossless when `usize` is at least 64 bits wide. The engine is
// 64-bit only, so reject narrower targets at compile time rather than silently
// truncate the generation (which would alias snapshot keys across nodes).
#[cfg(not(target_pointer_width = "64"))]
compile_error!(
    "oxidepage-dom requires a 64-bit target: OpaqueNode packs a NodeId's index \
     and generation into a single usize (M2)"
);

/// Packs a [`NodeId`] into stylo's [`OpaqueNode`] (a `usize`), losslessly on
/// 64-bit targets so snapshot keys can be mapped back to nodes.
pub(crate) fn opaque_node(id: NodeId) -> OpaqueNode {
    OpaqueNode((id.index() as usize) | ((id.generation().get() as usize) << 32))
}

/// Inverse of [`opaque_node`]. Public so the style engine can recover the
/// [`NodeId`] of an element stylo's traversal visited (it only hands out
/// `OpaqueNode`s), which is how a restyle reports the nodes it touched.
#[must_use]
pub fn node_id_from_opaque(opaque: OpaqueNode) -> NodeId {
    let bits = opaque.0;
    let index = (bits & 0xFFFF_FFFF) as u32;
    let generation =
        NonZeroU32::new((bits >> 32) as u32).expect("opaque node carries a non-zero generation");
    NodeId::from_parts(index, generation)
}

impl<'a> NodeRef<'a> {
    /// A handle to another node in the same tree.
    fn with(&self, id: NodeId) -> Self {
        self.with_node(id)
    }

    fn raw(&self) -> &'a crate::node::Node {
        self.tree().node(self.node)
    }
}

impl TDocument for NodeRef<'_> {
    type ConcreteNode = Self;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn is_html_document(&self) -> bool {
        self.tree().is_html_document(self.node)
    }

    fn quirks_mode(&self) -> QuirksMode {
        crate::select::selectors_quirks_mode_of(self.tree(), self.node)
    }

    fn shared_lock(&self) -> &SharedRwLock {
        self.tree().style_lock()
    }
}

impl NodeInfo for NodeRef<'_> {
    fn is_element(&self) -> bool {
        self.raw().data().kind() == NodeKind::Element
    }

    fn is_text_node(&self) -> bool {
        self.raw().is_text()
    }
}

impl TShadowRoot for NodeRef<'_> {
    type ConcreteNode = Self;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn host(&self) -> <Self::ConcreteNode as TNode>::ConcreteElement {
        let host = self
            .tree()
            .shadow_host(self.node)
            .expect("TShadowRoot::host on a non-shadow-root node");
        self.with(host)
    }

    fn style_data<'b>(&self) -> Option<&'b style::stylist::CascadeData>
    where
        Self: 'b,
    {
        // The style engine flushes each shadow root's `AuthorStyles` and
        // stores the resulting `Arc<CascadeData>` in the tree (ADR-0010);
        // the borrow lives as long as the active-tree scope.
        self.tree().shadow_cascade(self.node).map(|arc| &**arc)
    }
}

impl TNode for NodeRef<'_> {
    type ConcreteElement = Self;
    type ConcreteDocument = Self;
    type ConcreteShadowRoot = Self;

    fn parent_node(&self) -> Option<Self> {
        self.raw().parent().map(|id| self.with(id))
    }

    fn first_child(&self) -> Option<Self> {
        self.raw().first_child().map(|id| self.with(id))
    }

    fn last_child(&self) -> Option<Self> {
        self.raw().last_child().map(|id| self.with(id))
    }

    fn prev_sibling(&self) -> Option<Self> {
        self.raw().prev_sibling().map(|id| self.with(id))
    }

    fn next_sibling(&self) -> Option<Self> {
        self.raw().next_sibling().map(|id| self.with(id))
    }

    fn owner_doc(&self) -> Self::ConcreteDocument {
        // The node's *own* document, not the top-level one: with nested
        // browsing contexts they differ, and stylo asks this to reach the
        // quirks mode a frame's own cascade must use (ADR-0035 D1).
        self.with(self.tree().node_document(self.node))
    }

    fn is_in_document(&self) -> bool {
        self.raw().is_connected()
    }

    fn traversal_parent(&self) -> Option<Self::ConcreteElement> {
        // The restyle traversal walks the *flat* tree (stylo contract): an
        // assigned node's parent is its slot, a shadow child's parent is the
        // host, and an unassigned light child of a host has no parent.
        let tree = self.tree();
        let mut current = tree.flat_tree_parent(self.node)?;
        // A shadow-root fragment sits between host and shadow children in the
        // node tree but not in the flat tree: hop to the host element.
        if let Some(host) = tree.shadow_host(current) {
            current = host;
        }
        self.with(current).as_element()
    }

    fn opaque(&self) -> OpaqueNode {
        opaque_node(self.node)
    }

    fn debug_id(self) -> usize {
        self.node.index() as usize
    }

    fn as_element(&self) -> Option<Self::ConcreteElement> {
        (self.raw().data().kind() == NodeKind::Element).then_some(*self)
    }

    fn as_document(&self) -> Option<Self::ConcreteDocument> {
        (self.raw().data().kind() == NodeKind::Document).then_some(*self)
    }

    fn as_shadow_root(&self) -> Option<Self::ConcreteShadowRoot> {
        self.tree().is_shadow_root(self.node).then_some(*self)
    }
}

impl AttributeProvider for NodeRef<'_> {
    fn get_attr(&self, attr: &style::LocalName, _ns: &style::Namespace) -> Option<String> {
        // TODO: filter by namespace (matches blitz's current behavior).
        self.raw()
            .as_element()?
            .attrs()
            .iter()
            .find(|a| a.name.local == attr.0)
            .map(|a| a.value.to_string())
    }
}

impl<'a> TElement for NodeRef<'a> {
    type ConcreteNode = Self;
    type TraversalChildrenIterator = Traverser<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn traversal_children(&self) -> LayoutIterator<Self::TraversalChildrenIterator> {
        // Flat-tree children. Only a shadow host or a `<slot>` diverges from
        // the plain child list (slot jumps and host → shadow-children hops
        // are not expressible as sibling links, so those materialize a Vec);
        // every other element keeps the allocation-free lazy sibling walk.
        let el = self.element();
        let needs_flat = el.shadow_root().is_some()
            || (el.is_html_element() && el.name.local == local_name!("slot"));
        LayoutIterator(if needs_flat {
            Traverser::Flat {
                parent: *self,
                children: self.tree().flat_tree_children(self.node),
                index: 0,
            }
        } else {
            Traverser::Siblings {
                parent: *self,
                next: self.raw().first_child(),
            }
        })
    }

    fn inheritance_parent(&self) -> Option<Self> {
        // CSS inheritance follows the flat tree: slotted content inherits
        // through its slot, shadow children inherit from the host.
        TNode::traversal_parent(self)
    }

    fn is_html_element(&self) -> bool {
        self.element().is_html_element()
    }

    fn is_mathml_element(&self) -> bool {
        false
    }

    fn is_svg_element(&self) -> bool {
        self.element().name.ns == html5ever::ns!(svg)
    }

    fn style_attribute(&self) -> Option<ArcBorrow<'_, Locked<PropertyDeclarationBlock>>> {
        self.element()
            .stylo
            .style_attribute
            .as_ref()
            .map(|a| a.borrow_arc())
    }

    fn state(&self) -> ElementState {
        self.element().stylo.element_state
    }

    fn has_part_attr(&self) -> bool {
        self.element()
            .attr(&crate::node::attr_name(local_name!("part")))
            .is_some()
    }

    fn exports_any_part(&self) -> bool {
        // `exportparts` is a v1 limitation (ADR-0010).
        false
    }

    fn each_part<F>(&self, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        if let Some(value) = self
            .element()
            .attr(&crate::node::attr_name(local_name!("part")))
        {
            for token in value.split_ascii_whitespace() {
                callback(&AtomIdent::from(token));
            }
        }
    }

    fn id(&self) -> Option<&style::Atom> {
        self.element().id()
    }

    fn each_class<F>(&self, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        for class in self.element().classes() {
            callback(AtomIdent::cast(class));
        }
    }

    fn each_custom_state<F>(&self, _callback: F)
    where
        F: FnMut(&AtomIdent),
    {
    }

    fn each_attr_name<F>(&self, mut callback: F)
    where
        F: FnMut(&style::LocalName),
    {
        for attr in self.element().attrs() {
            callback(&GenericAtomIdent(attr.name.local.clone()));
        }
    }

    fn has_dirty_descendants(&self) -> bool {
        self.element().stylo.dirty_descendants.get()
    }

    fn has_snapshot(&self) -> bool {
        self.element().stylo.has_snapshot.get()
    }

    fn handled_snapshot(&self) -> bool {
        self.element().stylo.snapshot_handled.load(Ordering::SeqCst)
    }

    unsafe fn set_handled_snapshot(&self) {
        self.element()
            .stylo
            .snapshot_handled
            .store(true, Ordering::SeqCst);
    }

    unsafe fn set_dirty_descendants(&self) {
        self.element().stylo.dirty_descendants.set(true);
    }

    unsafe fn unset_dirty_descendants(&self) {
        self.element().stylo.dirty_descendants.set(false);
    }

    fn store_children_to_process(&self, _n: isize) {
        // Only stylo's *parallel* work-stealing traversal uses this counter.
        // The engine pins `layout.threads = 1` (`DomTree::init_stylo_prefs`),
        // so the sequential driver never calls it. Degrade to a no-op rather
        // than panicking in release if that pref ever regresses; a debug build
        // still trips the assertion below so the misconfiguration is caught in
        // tests (L4).
        #[cfg(debug_assertions)]
        unreachable!("store_children_to_process requires parallel traversal (layout.threads > 1)");
    }

    fn did_process_child(&self) -> isize {
        // Mirror of `store_children_to_process`: unreachable while
        // `layout.threads = 1`. In release, returning 0 ("no children remain")
        // is the safe default for the sequential traversal that never fills the
        // counter; a debug build panics to flag the misconfiguration (L4).
        #[cfg(debug_assertions)]
        unreachable!("did_process_child requires parallel traversal (layout.threads > 1)");
        #[cfg(not(debug_assertions))]
        0
    }

    unsafe fn ensure_data(&self) -> ElementDataMut<'_> {
        // SAFETY: stylo's traversal has exclusive access to this node.
        unsafe { self.element().stylo.data.ensure_init() }
    }

    unsafe fn clear_data(&self) {
        // SAFETY: stylo's traversal has exclusive access to this node.
        unsafe { self.element().stylo.data.clear() }
    }

    fn has_data(&self) -> bool {
        self.element().stylo.data.has_data()
    }

    fn borrow_data(&self) -> Option<ElementDataRef<'_>> {
        self.element().stylo.data.get()
    }

    fn mutate_data(&self) -> Option<ElementDataMut<'_>> {
        // SAFETY: stylo's traversal has exclusive access to this node.
        unsafe { self.element().stylo.data.unsafe_stylo_only_mut() }
    }

    fn skip_item_display_fixup(&self) -> bool {
        false
    }

    fn may_have_animations(&self) -> bool {
        false
    }

    fn has_animations(&self, _context: &SharedStyleContext<'_>) -> bool {
        false
    }

    fn has_css_animations(
        &self,
        _context: &SharedStyleContext<'_>,
        _pseudo_element: Option<PseudoElement>,
    ) -> bool {
        false
    }

    fn has_css_transitions(
        &self,
        _context: &SharedStyleContext<'_>,
        _pseudo_element: Option<PseudoElement>,
    ) -> bool {
        false
    }

    fn animation_rule(
        &self,
        _context: &SharedStyleContext<'_>,
    ) -> Option<ServoArc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn transition_rule(
        &self,
        _context: &SharedStyleContext<'_>,
    ) -> Option<ServoArc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn shadow_root(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        self.element().shadow_root().map(|id| self.with(id))
    }

    fn containing_shadow(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        self.tree()
            .containing_shadow_root(self.node)
            .map(|id| self.with(id))
    }

    fn lang_attr(&self) -> Option<AttrValue> {
        None
    }

    fn match_element_lang(&self, _override_lang: Option<Option<AttrValue>>, _value: &Lang) -> bool {
        false
    }

    fn is_html_document_body_element(&self) -> bool {
        if self.element().name.local != local_name!("body") {
            return false;
        }
        // A `<body>` counts only as a direct child of *its own* document's
        // root `<html>`.
        let tree = self.tree();
        tree.document_element_of(tree.node_document(self.node))
            .and_then(|root| self.raw().parent().map(|p| p == root))
            .unwrap_or(false)
    }

    fn synthesize_presentational_hints_for_legacy_attributes<V>(
        &self,
        _visited_handling: VisitedHandlingMode,
        _hints: &mut V,
    ) where
        V: Push<ApplicableDeclarationBlock>,
    {
        // Presentational hints (`width=`, `bgcolor=`, …) are deferred to P6.
    }

    fn local_name(&self) -> &LocalName {
        &self.element().name.local
    }

    fn namespace(&self) -> &Namespace {
        &self.element().name.ns
    }

    fn query_container_size(
        &self,
        _display: &style::values::specified::Display,
    ) -> euclid::default::Size2D<Option<app_units::Au>> {
        // Container queries are not implemented (P6); this disables them.
        Default::default()
    }

    fn has_selector_flags(&self, flags: ElementSelectorFlags) -> bool {
        self.element().stylo.selector_flags.get().contains(flags)
    }

    fn relative_selector_search_direction(&self) -> ElementSelectorFlags {
        let flags = self.element().stylo.selector_flags.get();
        use ElementSelectorFlags as F;
        if flags.contains(F::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING) {
            F::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING
        } else if flags.contains(F::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR) {
            F::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR
        } else if flags.contains(F::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING) {
            F::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING
        } else {
            F::empty()
        }
    }

    fn implicit_scope_for_sheet_in_shadow_root(
        _opaque_host: selectors::OpaqueElement,
        _sheet_index: usize,
    ) -> Option<ImplicitScopeRoot> {
        None
    }

    fn compute_layout_damage(_old: &ComputedValues, _new: &ComputedValues) -> RestyleDamage {
        // Layout is Phase 5; force full reconstruction until then.
        RestyleDamage::reconstruct()
    }
}

/// Iterator over an element's *flat-tree* children as [`NodeRef`]s, for stylo
/// traversal. Ordinary elements walk sibling links lazily (no allocation);
/// only shadow hosts and slots materialize their child list up front, because
/// slot jumps and host → shadow-children hops cannot be expressed as sibling
/// links.
///
/// Carries the parent handle, which witnesses the active-tree guard borrow for
/// `'a` and hands out siblings over the same tree. That borrow relies on arena
/// node addresses staying stable: no node may be allocated while a traversal is
/// in flight, or a `Vec` regrow would move existing nodes and dangle the `&Node`
/// references handed out here and by
/// [`NodeRef::opaque`](selectors::Element::opaque) (L2). Arena growth only
/// happens through the `&mut DomTree` mutation path, which the guard's shared
/// borrow of the tree excludes.
pub enum Traverser<'a> {
    /// Lazy sibling walk (the flat children equal the DOM children).
    Siblings {
        parent: NodeRef<'a>,
        next: Option<NodeId>,
    },
    /// Materialized flat-tree children (shadow host or `<slot>`).
    Flat {
        parent: NodeRef<'a>,
        children: Vec<NodeId>,
        index: usize,
    },
}

impl<'a> Iterator for Traverser<'a> {
    type Item = NodeRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Traverser::Siblings { parent, next } => {
                let current = (*next)?;
                *next = parent.tree().node(current).next_sibling();
                Some(parent.with_node(current))
            }
            Traverser::Flat {
                parent,
                children,
                index,
            } => {
                let current = *children.get(*index)?;
                *index += 1;
                Some(parent.with_node(current))
            }
        }
    }
}
