//! Selector matching over the arena DOM via Servo's `selectors` crate, using
//! stylo's own [`SelectorImpl`] (design doc §3.2, ADR-0005): the very same
//! [`NodeRef`] handle backs both `querySelector` and stylo's cascade (Phase 4),
//! so the two can never disagree about what matches.
//!
//! Non-tree-structural pseudo-classes (`:hover`, `:focus`, …) now *parse*
//! (stylo's grammar accepts them) but match nothing, because the element state
//! is empty until interactivity exists (P6). Pseudo-elements never match on a
//! real element.

use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;
use std::sync::OnceLock;

use html5ever::{LocalName, Namespace, local_name};
use oxidepage_base::{DomException, DomExceptionKind, NodeId};
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::{BLOOM_HASH_MASK, BloomFilter};
use selectors::context::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags,
    QuirksMode as SelectorsQuirksMode, SelectorCaches,
};
use selectors::matching::{ElementSelectorFlags, matches_selector_list};
use selectors::{OpaqueElement, SelectorList};
use style::CaseSensitivityExt;
use style::selector_parser::{NonTSPseudoClass, PseudoElement, SelectorImpl, SelectorParser};
use style::stylesheets::UrlExtraData;
use style::values::{AtomIdent, AtomString};
use style::{LocalName as StyleLocalName, Namespace as StyleNamespace};

use crate::node::{ElementData, NodeKind, is_text_kind};
use crate::tree::DomTree;

/// A parsed, reusable selector list.
pub struct CompiledSelectorList(pub(crate) SelectorList<SelectorImpl>);

/// URL data for parsing `querySelector` selector lists. The URL only matters
/// for chrome-privilege checks that never apply here, so a shared `about:blank`
/// instance suffices.
fn dummy_url_data() -> &'static UrlExtraData {
    static URL: OnceLock<UrlExtraData> = OnceLock::new();
    URL.get_or_init(|| {
        UrlExtraData::from(::url::Url::parse("about:blank").expect("about:blank parses"))
    })
}

/// Parses a selector list per `querySelector*` (a failure is a `SyntaxError`
/// DOMException, as the spec requires).
pub fn parse_selector_list(input: &str) -> Result<CompiledSelectorList, DomException> {
    SelectorParser::parse_author_origin_no_namespace(input, dummy_url_data())
        .map(CompiledSelectorList)
        .map_err(|_| {
            DomException::new(
                DomExceptionKind::SyntaxError,
                "failed to parse selector list",
            )
        })
}

thread_local! {
    /// The [`DomTree`] backing the [`NodeRef`] handles currently in use.
    ///
    /// Stylo requires its element handle to be pointer-sized (one `usize`): its
    /// style-sharing cache is a fixed-size buffer sized for a single-pointer
    /// element (ADR-0005, mirroring `blitz-dom`'s `&Node`). Our natural handle
    /// `(&DomTree, NodeId)` is two words, so we store only the `NodeId` in
    /// [`NodeRef`] and recover the tree from this thread-local, which is
    /// installed for the duration of a query or style traversal.
    static ACTIVE_TREE: Cell<*const DomTree> = const { Cell::new(std::ptr::null()) };
}

/// RAII guard restoring the previous active tree when dropped.
///
/// The `'a` lifetime ties the guard to the borrow of the [`DomTree`] passed to
/// [`enter_active_tree`]: while the guard is alive, the tree cannot be moved,
/// dropped, or mutably borrowed. [`NodeRef`] handles in turn borrow *the guard*,
/// so the guard cannot be dropped while a handle is alive either. Together these
/// make "a live handle implies the installed tree is live" — the invariant the
/// pointer-sized-handle scheme relies on for [`NodeRef::tree`] to be sound
/// (ADR-0005) — a compile-time guarantee rather than a convention.
///
/// The tree cannot be moved out from under the guard:
///
/// ```compile_fail
/// use oxidepage_dom::{ParseOptions, parse_document};
/// use oxidepage_dom::select::enter_active_tree;
/// let tree = parse_document("<div></div>", ParseOptions::default()).tree;
/// let guard = enter_active_tree(&tree);
/// drop(tree); // ERROR: cannot move `tree` while the guard borrows it.
/// drop(guard);
/// ```
///
/// …and a handle cannot outlive the scope that installed the tree, which would
/// otherwise leave [`NodeRef::tree`] dereferencing a null thread-local:
///
/// ```compile_fail
/// use oxidepage_dom::{ParseOptions, parse_document};
/// use oxidepage_dom::select::{NodeRef, enter_active_tree};
/// let tree = parse_document("<div></div>", ParseOptions::default()).tree;
/// let guard = enter_active_tree(&tree);
/// let node = NodeRef::new(&guard, tree.document());
/// drop(guard); // ERROR: cannot move `guard` while `node` borrows it.
/// let _ = node.node_id();
/// ```
#[must_use = "the active tree is only installed while the guard is alive"]
pub struct ActiveTreeGuard<'a> {
    prev: *const DomTree,
    _tree: PhantomData<&'a DomTree>,
}

impl Drop for ActiveTreeGuard<'_> {
    fn drop(&mut self) {
        ACTIVE_TREE.with(|c| c.set(self.prev));
    }
}

/// Installs `tree` as the active tree for [`NodeRef`] handles until the
/// returned guard is dropped. The guard borrows `tree`, so the borrow checker
/// keeps it alive and unmoved for the guard's (and any derived handle's)
/// lifetime.
///
/// Entering a nested scope with a *different* tree is a bug: outer [`NodeRef`]
/// handles would silently resolve against the inner tree's arena (M1). This is
/// caught by a `debug_assert!`; nesting the *same* tree is fine (the guard
/// restores the previous pointer on drop).
pub fn enter_active_tree<'a>(tree: &'a DomTree) -> ActiveTreeGuard<'a> {
    let new = std::ptr::from_ref(tree);
    let prev = ACTIVE_TREE.with(|c| c.replace(new));
    debug_assert!(
        prev.is_null() || prev == new,
        "nested enter_active_tree with a different DomTree: outer NodeRef \
         handles would resolve against the inner tree's arena (M1)"
    );
    ActiveTreeGuard {
        prev,
        _tree: PhantomData,
    }
}

/// A node handle over the arena, as `selectors` and stylo see it.
///
/// A single `Copy`, pointer-sized handle serves every role (node, element,
/// document); the trait methods check the node kind where it matters, mirroring
/// `blitz-dom`'s `BlitzNode`. The backing tree comes from [`ACTIVE_TREE`].
#[derive(Clone, Copy)]
pub struct NodeRef<'a> {
    pub(crate) node: NodeId,
    _tree: PhantomData<&'a DomTree>,
}

impl fmt::Debug for NodeRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeRef({:?})", self.node)
    }
}

impl<'a> NodeRef<'a> {
    /// Wraps `node` as a stylo/selectors handle, borrowing the [`ActiveTreeGuard`]
    /// that installed the tree.
    ///
    /// Borrowing the guard (rather than the tree) is what makes [`Self::tree`]
    /// sound: the handle cannot outlive the scope whose thread-local it reads.
    /// Taking `&'a DomTree` here instead would let safe code drop the guard while
    /// a handle lives, and `tree()` would then dereference a null pointer.
    #[must_use]
    pub fn new(guard: &'a ActiveTreeGuard<'_>, node: NodeId) -> Self {
        let _ = guard;
        Self {
            node,
            _tree: PhantomData,
        }
    }

    /// A sibling handle over the same tree, for the `selectors`/stylo traversal
    /// methods. Sound without a guard argument because `self` already witnesses
    /// a live guard borrow for `'a`.
    pub(crate) fn with_node(self, node: NodeId) -> Self {
        Self {
            node,
            _tree: PhantomData,
        }
    }

    /// The node id this handle points at.
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        self.node
    }

    /// The backing tree, recovered from the active-tree scope.
    #[allow(unsafe_code)]
    pub(crate) fn tree(&self) -> &'a DomTree {
        let ptr = ACTIVE_TREE.with(Cell::get);
        debug_assert!(!ptr.is_null(), "NodeRef used outside an active-tree scope");
        // SAFETY: constructing a `NodeRef<'a>` requires borrowing an
        // `ActiveTreeGuard` for `'a` (`NodeRef::new`), or holding one that
        // already does (`with_node`). The guard therefore outlives this handle,
        // the pointer it installed is non-null, and the tree it points at is
        // immutably borrowed for at least `'a` (ADR-0005).
        unsafe { &*ptr }
    }

    /// The element payload; panics if this handle does not wrap an element.
    pub(crate) fn element(&self) -> &'a ElementData {
        self.tree()
            .node(self.node)
            .as_element()
            .expect("NodeRef used as an element must wrap an element")
    }

    /// True if this element is a hyperlink source (`a`/`area` with `href`).
    fn is_link_element(&self) -> bool {
        let el = self.element();
        (el.name.local == local_name!("a") || el.name.local == local_name!("area"))
            && el
                .attr(&crate::node::attr_name(local_name!("href")))
                .is_some()
    }
}

impl PartialEq for NodeRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        // Handles only ever compare within one active tree, so id identity
        // (index + generation) is sufficient.
        self.node == other.node
    }
}

impl Eq for NodeRef<'_> {}

impl std::hash::Hash for NodeRef<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.node.hash(state);
    }
}

impl<'a> selectors::Element for NodeRef<'a> {
    type Impl = SelectorImpl;

    fn opaque(&self) -> OpaqueElement {
        // Identity is the `Node`'s heap address. This is only stable while the
        // arena does not reallocate: no node may be allocated during a
        // selector match or style traversal, or a `Vec` regrow would move
        // existing nodes and change their addresses mid-pass (L2). All arena
        // growth happens through the `&mut DomTree` mutation path, which cannot
        // run while an `ActiveTreeGuard` borrows the tree.
        OpaqueElement::new(self.tree().node(self.node))
    }

    fn parent_element(&self) -> Option<Self> {
        let parent = self.tree().node(self.node).parent()?;
        (self.tree().node(parent).data().kind() == NodeKind::Element)
            .then(|| self.with_node(parent))
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        self.tree()
            .node(self.node)
            .parent()
            .is_some_and(|p| self.tree().is_shadow_root(p))
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        let root = self.tree().containing_shadow_root(self.node)?;
        let host = self.tree().shadow_host(root)?;
        Some(self.with_node(host))
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        let mut current = self.tree().node(self.node).prev_sibling();
        while let Some(id) = current {
            if self.tree().node(id).data().kind() == NodeKind::Element {
                return Some(self.with_node(id));
            }
            current = self.tree().node(id).prev_sibling();
        }
        None
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let mut current = self.tree().node(self.node).next_sibling();
        while let Some(id) = current {
            if self.tree().node(id).data().kind() == NodeKind::Element {
                return Some(self.with_node(id));
            }
            current = self.tree().node(id).next_sibling();
        }
        None
    }

    fn first_element_child(&self) -> Option<Self> {
        self.tree()
            .children(self.node)
            .find(|&c| self.tree().node(c).data().kind() == NodeKind::Element)
            .map(|c| self.with_node(c))
    }

    fn is_html_element_in_html_document(&self) -> bool {
        self.element().is_html_element()
    }

    fn has_local_name(&self, local_name: &LocalName) -> bool {
        self.element().name.local == *local_name
    }

    fn has_namespace(&self, ns: &Namespace) -> bool {
        self.element().name.ns == *ns
    }

    fn is_same_type(&self, other: &Self) -> bool {
        let (a, b) = (self.element(), other.element());
        a.name.local == b.name.local && a.name.ns == b.name.ns
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&StyleNamespace>,
        local_name: &StyleLocalName,
        operation: &AttrSelectorOperation<&AtomString>,
    ) -> bool {
        self.element().attrs().iter().any(|attr| {
            if attr.name.local != local_name.0 {
                return false;
            }
            match ns {
                NamespaceConstraint::Any => {}
                NamespaceConstraint::Specific(url) => {
                    if attr.name.ns != url.0 {
                        return false;
                    }
                }
            }
            operation.eval_str(&attr.value)
        })
    }

    fn match_non_ts_pseudo_class(
        &self,
        pseudo_class: &NonTSPseudoClass,
        _context: &mut MatchingContext<'_, Self::Impl>,
    ) -> bool {
        match pseudo_class {
            // Visitedness is deliberately not tracked (it is a privacy leak and
            // meaningless headless), so the link pseudo-classes are answered
            // structurally instead of from the VISITED/UNVISITED bits that
            // `state_flag()` would name.
            NonTSPseudoClass::AnyLink | NonTSPseudoClass::Link => self.is_link_element(),
            NonTSPseudoClass::Visited => false,
            // Everything else stylo supports is a pure element-state bit.
            // Deferring to stylo's own pseudo-class → bit mapping (rather than
            // repeating it here) is what keeps the two from drifting: a
            // pseudo-class whose bit we never set simply never matches, which
            // is the honest answer, and one we later start setting starts
            // matching without a second edit here.
            //
            // `state_flag()` is empty for `:lang()`, `:state()` and
            // `-servo-non-zero-border`, none of which we implement.
            other => {
                let flag = other.state_flag();
                !flag.is_empty() && self.element().stylo.element_state.contains(flag)
            }
        }
    }

    fn match_pseudo_element(
        &self,
        _pe: &PseudoElement,
        _context: &mut MatchingContext<'_, Self::Impl>,
    ) -> bool {
        false
    }

    fn apply_selector_flags(&self, flags: ElementSelectorFlags) {
        let self_flags = flags.for_self();
        if !self_flags.is_empty() {
            let cell = &self.element().stylo.selector_flags;
            cell.set(cell.get() | self_flags);
        }
        let parent_flags = flags.for_parent();
        if !parent_flags.is_empty()
            && let Some(parent) = self.parent_element()
        {
            let cell = &parent.element().stylo.selector_flags;
            cell.set(cell.get() | parent_flags);
        }
    }

    fn is_link(&self) -> bool {
        // `:link`/`:any-link` match `a`/`area`/`link` with an `href`; reuse the
        // one definition of "hyperlink" so the selector engine and the cascade
        // never disagree.
        self.is_link_element()
    }

    fn is_html_slot_element(&self) -> bool {
        let el = self.element();
        el.is_html_element() && el.name.local == local_name!("slot")
    }

    fn assigned_slot(&self) -> Option<Self> {
        self.tree()
            .assigned_slot(self.node)
            .map(|slot| self.with_node(slot))
    }

    fn has_id(&self, id: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        self.element()
            .id()
            .is_some_and(|el_id| case_sensitivity.eq_atom(el_id, id))
    }

    fn has_class(&self, name: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        self.element()
            .classes()
            .iter()
            .any(|class| case_sensitivity.eq_atom(class, name))
    }

    fn has_custom_state(&self, _name: &AtomIdent) -> bool {
        false
    }

    fn imported_part(&self, _name: &AtomIdent) -> Option<AtomIdent> {
        // `exportparts` forwarding is a v1 limitation (ADR-0010).
        None
    }

    fn is_part(&self, name: &AtomIdent) -> bool {
        self.element()
            .attr(&crate::node::attr_name(local_name!("part")))
            .is_some_and(|value| {
                value
                    .split_ascii_whitespace()
                    .any(|token| *token == ***name)
            })
    }

    fn is_empty(&self) -> bool {
        self.tree().children(self.node).all(|c| {
            let node = self.tree().node(c);
            match node.data().kind() {
                NodeKind::Element => false,
                kind if is_text_kind(kind) => node.character_data().is_none_or(|d| d.is_empty()),
                _ => true,
            }
        })
    }

    fn is_root(&self) -> bool {
        self.tree()
            .node(self.node)
            .parent()
            .is_some_and(|p| self.tree().node(p).data().kind() == NodeKind::Document)
    }

    fn add_element_unique_hashes(&self, filter: &mut BloomFilter) -> bool {
        style::bloom::each_relevant_element_hash(*self, |hash| {
            filter.insert_hash(hash & BLOOM_HASH_MASK);
        });
        true
    }
}

/// `doc`'s quirks mode in the selectors crate's spelling.
///
/// Per document, not per tree: with nested browsing contexts an iframe can be
/// in quirks mode while its embedder is not (ADR-0035 D1).
pub(crate) fn selectors_quirks_mode_of(tree: &DomTree, doc: NodeId) -> SelectorsQuirksMode {
    match tree.quirks_mode_of(doc) {
        html5ever::interface::QuirksMode::Quirks => SelectorsQuirksMode::Quirks,
        html5ever::interface::QuirksMode::LimitedQuirks => SelectorsQuirksMode::LimitedQuirks,
        html5ever::interface::QuirksMode::NoQuirks => SelectorsQuirksMode::NoQuirks,
    }
}

fn matches_list(tree: &DomTree, element: NodeId, list: &CompiledSelectorList) -> bool {
    let scope = enter_active_tree(tree);
    let mut caches = SelectorCaches::default();
    let mut context = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches,
        selectors_quirks_mode_of(tree, tree.node_document(element)),
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );
    matches_selector_list(&list.0, &NodeRef::new(&scope, element), &mut context)
}

impl DomTree {
    /// Spec `Element.matches()`.
    #[must_use]
    pub fn element_matches(&self, element: NodeId, selectors: &CompiledSelectorList) -> bool {
        matches_list(self, element, selectors)
    }

    /// Spec `Element.closest()`.
    #[must_use]
    pub fn closest(&self, element: NodeId, selectors: &CompiledSelectorList) -> Option<NodeId> {
        self.inclusive_ancestors(element)
            .filter(|&id| self.node(id).data().kind() == NodeKind::Element)
            .find(|&id| matches_list(self, id, selectors))
    }

    /// Spec `querySelector`: first matching element descendant of `root`,
    /// in tree order.
    #[must_use]
    pub fn query_selector(&self, root: NodeId, selectors: &CompiledSelectorList) -> Option<NodeId> {
        self.inclusive_descendants(root)
            .skip(1)
            .filter(|&id| self.node(id).data().kind() == NodeKind::Element)
            .find(|&id| matches_list(self, id, selectors))
    }

    /// Spec `querySelectorAll` (returns a static snapshot).
    #[must_use]
    pub fn query_selector_all(
        &self,
        root: NodeId,
        selectors: &CompiledSelectorList,
    ) -> Vec<NodeId> {
        self.inclusive_descendants(root)
            .skip(1)
            .filter(|&id| self.node(id).data().kind() == NodeKind::Element)
            .filter(|&id| matches_list(self, id, selectors))
            .collect()
    }
}
