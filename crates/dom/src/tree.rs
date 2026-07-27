//! The arena DOM tree and the spec mutation algorithms.
//!
//! There is exactly one code path for every mutation (design doc §5.2): the
//! internal `insert_internal` / `remove_internal` / attribute / character-data
//! primitives each (a) update the tree, (b) queue `MutationObserver` records,
//! (c) run the invalidation hook (dirty bits up the ancestor chain — stylo
//! restyle hints attach here in Phase 4). The public spec algorithms
//! (`pre_insert`, `replace_child`, …) validate and then delegate to those
//! primitives, so invalidation can never be forgotten by a caller.
//!
//! Removed subtrees stay in the arena as detached trees (script can
//! re-insert them; the parser holds handles to detached nodes). Freeing is
//! explicit: the JS wrapper pin contract (design doc §5.3) drives
//! [`DomTree::free_detached_tree_if_unpinned`], and [`DomTree::free_subtree`]
//! refuses to free pinned nodes.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use html5ever::interface::QuirksMode;
use html5ever::tendril::StrTendril;
use html5ever::{Attribute, QualName, local_name, ns};
use oxidepage_base::id::FIRST_GENERATION;
use oxidepage_base::{DomException, DomExceptionKind, NodeId};
use style::Atom;
use style::attr::{AttrIdentifier, AttrValue};
use style::invalidation::element::restyle_hints::RestyleHint;
use style::selector_parser::{ServoElementSnapshot, SnapshotMap};
use style::shared_lock::SharedRwLock;
use style::stylesheets::UrlExtraData;
use style::values::GenericAtomIdent;

use crate::arena::Arena;
use crate::custom_element::{
    CustomElementReaction, CustomElementState, is_valid_custom_element_name,
};
use crate::event::ListenerRegistry;
use crate::node::{DocumentData, ElementData, Node, NodeData, NodeFlags, NodeKind, is_text_kind};
use crate::observer::{MutationObserverRegistry, RecordContents, RecordKind};
use crate::shadow::{ShadowMode, is_valid_shadow_host_name};
use crate::stylo::opaque_node;

/// A stylesheet-owning element whose relevance to the style set changed and
/// that the style engine must (re)process. Drained via
/// [`DomTree::take_style_updates`] after each parser run and script task.
///
/// The wrapped [`NodeId`] is a *snapshot*: the node may have been removed and
/// its arena slot recycled between queuing and draining. Generation-tagged
/// ids make a recycled slot resolve to `None`, so consumers MUST validate
/// liveness at the drain boundary (e.g. via [`DomTree::get`]) before acting on
/// the id (L3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleUpdate {
    /// A `<style>` element gained or changed its text/attributes.
    StyleElement(NodeId),
    /// A `<style>` element left the document.
    StyleElementRemoved(NodeId),
    /// A `<link rel="stylesheet">` element became relevant.
    LinkElement(NodeId),
    /// A `<link rel="stylesheet">` element left the document.
    LinkElementRemoved(NodeId),
}

/// Enables the stylo layout-feature prefs before any CSS parsing happens.
///
/// Inline `style=""` attributes are parsed during *document* parsing, which
/// can run before a `StyleEngine` exists — pref-gated properties (grid,
/// columns, …) would silently drop from those declarations. `StyleEngine::new`
/// sets the same prefs; both paths are idempotent.
fn init_stylo_prefs() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        style_config::set_pref!("layout.grid.enabled", true);
        style_config::set_pref!("layout.columns.enabled", true);
        style_config::set_pref!("layout.css.basic-shape-shape.enabled", true);
        style_config::set_pref!("layout.threads", 1);
    });
}

/// Attributes whose mutation can only affect layout through the cascade
/// (`style`/`class`/`id`): computed-style pointer diffing catches those, so
/// they do not bump the structure version. Everything else (`width`,
/// `colspan`, `type`, …) may feed box-tree construction directly.
fn is_style_only_attr(local: &html5ever::LocalName) -> bool {
    matches!(&**local, "style" | "class" | "id")
}

/// The namespace URI of an attribute name as an `Option<String>`, `None` for
/// the null namespace (the common case, and what `attributeChangedCallback`
/// receives for HTML attributes).
fn attr_namespace(name: &QualName) -> Option<String> {
    if name.ns == ns!() {
        None
    } else {
        Some(name.ns.to_string())
    }
}

/// Parses `url` for stylo's [`UrlExtraData`], falling back to `about:blank`.
fn make_url_extra_data(url: &str) -> UrlExtraData {
    let parsed = ::url::Url::parse(url)
        .unwrap_or_else(|_| ::url::Url::parse("about:blank").expect("about:blank is a valid URL"));
    UrlExtraData::from(parsed)
}

/// Builds a stylo attribute snapshot of `el`'s current attributes, mirroring
/// `blitz-dom`'s `snapshot_node`. All change flags are set conservatively so
/// invalidation reprocesses the element fully.
fn build_snapshot(el: &ElementData) -> ServoElementSnapshot {
    let attrs: Vec<(AttrIdentifier, AttrValue)> = el
        .attrs()
        .iter()
        .map(|attr| {
            let ident = AttrIdentifier {
                local_name: GenericAtomIdent(attr.name.local.clone()),
                name: GenericAtomIdent(attr.name.local.clone()),
                namespace: GenericAtomIdent(attr.name.ns.clone()),
                prefix: attr.name.prefix.clone().map(GenericAtomIdent),
            };
            let value = if attr.name.local == local_name!("id") {
                AttrValue::Atom(Atom::from(&*attr.value))
            } else if attr.name.local == local_name!("class") {
                let classes = attr
                    .value
                    .split_ascii_whitespace()
                    .map(Atom::from)
                    .collect();
                AttrValue::TokenList(OnceLock::from(attr.value.to_string()), classes)
            } else {
                AttrValue::String(attr.value.to_string())
            };
            (ident, value)
        })
        .collect();
    let changed_attrs = attrs.iter().map(|(ident, _)| ident.name.clone()).collect();
    ServoElementSnapshot {
        state: Some(el.stylo.element_state),
        attrs: Some(attrs),
        changed_attrs,
        class_changed: true,
        id_changed: true,
        other_attributes_changed: true,
    }
}

/// `compareDocumentPosition` result bits (DOM spec §4.4).
pub const DOCUMENT_POSITION_DISCONNECTED: u16 = 0x01;
pub const DOCUMENT_POSITION_PRECEDING: u16 = 0x02;
pub const DOCUMENT_POSITION_FOLLOWING: u16 = 0x04;
pub const DOCUMENT_POSITION_CONTAINS: u16 = 0x08;
pub const DOCUMENT_POSITION_CONTAINED_BY: u16 = 0x10;
pub const DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC: u16 = 0x20;

/// One shadow root's slot lookup: the `structure_version` it was built at,
/// plus the first `<slot>` per name in tree order.
type SlotMap = (u64, HashMap<String, NodeId>);

/// The single source of truth for document state.
pub struct DomTree {
    pub(crate) arena: Arena,
    document: NodeId,
    pub(crate) listeners: ListenerRegistry,
    pub(crate) observers: MutationObserverRegistry,
    /// JS-wrapper pin counts (design doc §5.3): a live wrapper pins its
    /// node; fully-unpinned detached trees may be freed.
    pins: HashMap<NodeId, u32>,
    /// The document's shared style lock (design doc §10). Locked stylo data —
    /// `style` attributes, stylesheet rules — is read/written under this lock;
    /// the style engine holds a clone.
    style_lock: SharedRwLock,
    /// The document URL as stylo's URL data, used to resolve relative URLs in
    /// parsed style attributes and selector parsing. Tracks the document URL.
    url_extra_data: UrlExtraData,
    /// Element snapshots taken *before* attribute/state mutations, consumed by
    /// stylo's incremental restyle invalidation (design doc §10).
    snapshots: SnapshotMap,
    /// Queue of stylesheet-owning elements whose relevance changed, drained by
    /// the style engine after each parser run / script task.
    style_updates: Vec<StyleUpdate>,
    /// Queue of `<img>` elements that became relevant (connected, or `src`
    /// changed), drained by the page to start image loads (Phase 6, WP-J).
    image_updates: Vec<NodeId>,
    /// Queue of `<script>` elements that became eligible for preparation after
    /// connection or a relevant pre-start mutation. Page owns execution/fetch.
    script_updates: Vec<NodeId>,
    /// Monotonic counter bumped on every style-relevant DOM mutation (all funnel
    /// through `note_children_changed`/`note_subtree_mutation`). Lets CSSOM views
    /// cache resolved values and invalidate them exactly when the DOM changes.
    style_version: Cell<u64>,
    /// Monotonic counter bumped on layout-structural mutations: child-list
    /// changes, character-data changes, and attribute changes other than
    /// `style`/`class`/`id` (which only affect styles, visible to layout via
    /// computed-style pointer diffing). The layout engine's incremental
    /// relayout falls back to a full box-tree rebuild when this moves.
    structure_version: Cell<u64>,
    /// Memoized document base URL, tagged with the `structure_version` it was
    /// computed at. `.href` reflection reads it once per anchor, so recomputing
    /// it (a tree walk looking for `<base href>`) on every read would be
    /// quadratic. `structure_version` moves on exactly the mutations that can
    /// change the answer — child-list changes and non-style attribute writes —
    /// so it doubles as the cache key. `set_document_url` is the one exception
    /// and clears the cache by hand.
    base_url_cache: RefCell<Option<(u64, String)>>,
    /// Connected elements by `id` attribute; detached elements are never in
    /// here. Values are unordered: tree order is resolved at lookup, and
    /// duplicate ids are rare enough that the comparison never runs in
    /// practice. Maintained incrementally — see [`DomTree::index_add`].
    ids: HashMap<String, Vec<NodeId>>,
    /// Bumped on every `ids` change, so consumers can cache derived state.
    /// `structure_version` cannot serve here: `is_style_only_attr` treats `id`
    /// as style-only, so an id change does not move it.
    id_version: Cell<u64>,
    /// Custom-element names for which the bindings layer has called `define`.
    /// The definitions themselves (constructors, callbacks) live in bindings;
    /// the DOM tracks only the *set of defined names* so it knows which
    /// `Undefined` elements to enqueue for upgrade.
    defined_names: HashSet<String>,
    /// FIFO queue of custom-element reaction intents (upgrade/connected/
    /// disconnected/attributeChanged). Drained by the bindings layer at each
    /// microtask checkpoint. Entries are `NodeId` snapshots; consumers must
    /// revalidate liveness before use (L3, same as `image_updates`).
    custom_reactions: Vec<CustomElementReaction>,
    /// Connectedness changes for *pinned* (JS-wrapped) nodes: `(id, connected)`.
    /// Drained by the bindings layer, which strongly retains a connected node's
    /// wrapper (so author-set expando properties survive GC — jQuery/Angular
    /// store data there) and releases it on disconnect (so detached subtrees
    /// still free). Only populated while wrappers exist (`pins` non-empty);
    /// entries are `NodeId` snapshots, revalidated at the drain boundary.
    pinned_connectivity: Vec<(NodeId, bool)>,
    /// Live shadow root fragments (maintained by [`DomTree::attach_shadow`]
    /// and [`DomTree::free_subtree`]). Layout bails out of incremental
    /// patching while any exist.
    shadow_roots: HashSet<NodeId>,
    /// Per-shadow-root cascade data, written by the style engine after each
    /// flush of the root's author styles and read back by stylo through
    /// `TShadowRoot::style_data` (side-map; ADR-0010).
    shadow_cascade: HashMap<NodeId, servo_arc::Arc<style::stylist::CascadeData>>,
    /// Per-shadow-root slot lookup (`name` → first `<slot>` in tree order),
    /// tagged with the `structure_version` it was built at. See
    /// [`DomTree::find_slot`].
    slot_cache: RefCell<HashMap<NodeId, SlotMap>>,
    /// The focused element (`document.activeElement`), source of the `:focus`
    /// and `:focus-within` element states. See [`crate::form`].
    pub(crate) focused: Option<NodeId>,
    /// The element the pointer is over, source of `:hover`. Like `focused`, the
    /// state applies to the whole inclusive-ancestor chain — `:hover` matches
    /// every ancestor of the hovered element, which is what makes a hover rule
    /// on a menu container work.
    pub(crate) hovered: Option<NodeId>,
    /// The element being pressed, source of `:active`. Set between `mousedown`
    /// and `mouseup`, and likewise inherited by ancestors.
    pub(crate) active: Option<NodeId>,
}

fn hierarchy_error(message: &'static str) -> DomException {
    DomException::new(DomExceptionKind::HierarchyRequestError, message)
}

impl Default for DomTree {
    fn default() -> Self {
        Self::new()
    }
}

impl DomTree {
    /// Creates a tree containing only a document node.
    #[must_use]
    pub fn new() -> Self {
        Self::with_generation_base(FIRST_GENERATION)
    }

    /// The generation base a tree replacing this one must be built with, so
    /// that ids of this document cannot alias nodes of the next one. See
    /// [`Arena::with_generation_base`].
    #[must_use]
    pub fn next_generation_base(&self) -> NonZeroU32 {
        self.arena.next_generation_base()
    }

    /// Creates a tree whose nodes are allocated above `base`, used to build the
    /// successor document at navigation.
    #[must_use]
    pub fn with_generation_base(base: NonZeroU32) -> Self {
        init_stylo_prefs();
        let mut arena = Arena::with_generation_base(base);
        let mut doc = Node::new(NodeData::Document(DocumentData::default()));
        doc.flags.insert(NodeFlags::IS_CONNECTED);
        let document = arena.alloc(doc);
        Self {
            arena,
            document,
            listeners: ListenerRegistry::default(),
            observers: MutationObserverRegistry::default(),
            pins: HashMap::new(),
            style_lock: SharedRwLock::new(),
            url_extra_data: make_url_extra_data("about:blank"),
            snapshots: SnapshotMap::new(),
            style_updates: Vec::new(),
            image_updates: Vec::new(),
            script_updates: Vec::new(),
            style_version: Cell::new(0),
            structure_version: Cell::new(0),
            base_url_cache: RefCell::new(None),
            ids: HashMap::new(),
            id_version: Cell::new(0),
            defined_names: HashSet::new(),
            custom_reactions: Vec::new(),
            pinned_connectivity: Vec::new(),
            shadow_roots: HashSet::new(),
            shadow_cascade: HashMap::new(),
            slot_cache: RefCell::new(HashMap::new()),
            focused: None,
            hovered: None,
            active: None,
        }
    }

    /// Records the flushed cascade data for a shadow root (style engine).
    pub fn set_shadow_cascade(
        &mut self,
        root: NodeId,
        data: servo_arc::Arc<style::stylist::CascadeData>,
    ) {
        self.shadow_cascade.insert(root, data);
    }

    /// The cascade data last flushed for a shadow root, if any.
    #[must_use]
    pub fn shadow_cascade(
        &self,
        root: NodeId,
    ) -> Option<&servo_arc::Arc<style::stylist::CascadeData>> {
        self.shadow_cascade.get(&root)
    }

    // === `id` index ===
    //
    // Invariant: `ids` contains exactly the connected elements carrying an
    // `id` attribute. Three call sites maintain it, and together they cover
    // every way an element can enter or leave that set:
    //
    //   * `propagate_connectedness` — the single connectedness hook, reached
    //     from both `insert_internal` and `remove_internal` (a move is a
    //     removal followed by an insertion, so it is covered too);
    //   * `set_attribute` / `remove_attribute` — the only paths that mutate
    //     `ElementData::id`.
    //
    // The proptest invariant in `tests/proptest_mutations.rs` cross-checks the
    // index against a linear scan after every mutation.

    fn index_add(&mut self, id: &str, node: NodeId) {
        let slot = self.ids.entry(id.to_owned()).or_default();
        if !slot.contains(&node) {
            slot.push(node);
            self.id_version.set(self.id_version.get() + 1);
        }
    }

    fn index_remove(&mut self, id: &str, node: NodeId) {
        let Some(slot) = self.ids.get_mut(id) else {
            return;
        };
        let Some(pos) = slot.iter().position(|&n| n == node) else {
            return;
        };
        slot.swap_remove(pos);
        if slot.is_empty() {
            self.ids.remove(id);
        }
        self.id_version.set(self.id_version.get() + 1);
    }

    /// The first connected element in tree order carrying `id`.
    ///
    /// O(1) for the overwhelmingly common case of a unique id. A duplicated id
    /// falls back to picking the tree-order minimum via
    /// [`DomTree::compare_document_position`], which is O(n) — the same cost as
    /// the linear scan this index replaces, and only for the duplicates.
    #[must_use]
    pub fn element_by_id(&self, id: &str) -> Option<NodeId> {
        let candidates = self.ids.get(id)?;
        match candidates.as_slice() {
            [] => None,
            [only] => Some(*only),
            [first, rest @ ..] => Some(rest.iter().fold(*first, |best, &other| {
                // Bits describe `other` relative to `best` (spec argument order).
                if self.compare_document_position(best, other) & DOCUMENT_POSITION_PRECEDING != 0 {
                    other
                } else {
                    best
                }
            })),
        }
    }

    /// The `id`s currently present in the document, in unspecified order.
    pub fn id_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.ids.keys().map(String::as_str)
    }

    /// A counter that increases whenever the set of connected element ids
    /// changes. Consumers deriving state from [`DomTree::id_names`] use it as
    /// a cache key.
    #[must_use]
    pub fn id_version(&self) -> u64 {
        self.id_version.get()
    }

    /// A counter that increases on every style-relevant DOM mutation. CSSOM
    /// computed-value views use it to validate their cache.
    #[must_use]
    pub fn style_version(&self) -> u64 {
        self.style_version.get()
    }

    /// A counter that increases on every layout-structural mutation (see the
    /// field docs). Style-only mutations do not move it.
    #[must_use]
    pub fn structure_version(&self) -> u64 {
        self.structure_version.get()
    }

    fn bump_structure_version(&self) {
        self.structure_version.set(self.structure_version.get() + 1);
    }

    /// The document's shared style lock (design doc §10).
    #[must_use]
    pub fn style_lock(&self) -> &SharedRwLock {
        &self.style_lock
    }

    /// The document URL as stylo's URL data (for relative-URL resolution).
    #[must_use]
    pub fn url_extra_data(&self) -> &UrlExtraData {
        &self.url_extra_data
    }

    /// The element snapshots accumulated since the last style resolution.
    #[must_use]
    pub fn snapshots(&self) -> &SnapshotMap {
        &self.snapshots
    }

    /// Drops all snapshots and clears each snapshotted element's `has_snapshot`
    /// bit. Called by the style engine after a resolution pass consumes them.
    pub fn clear_snapshots(&mut self) {
        let ids: Vec<NodeId> = self
            .snapshots
            .keys()
            .map(|opaque| crate::stylo::node_id_from_opaque(*opaque))
            .collect();
        for id in ids {
            if let Some(el) = self.get(id).and_then(Node::as_element) {
                el.stylo.has_snapshot.set(false);
            }
        }
        self.snapshots.clear();
    }

    /// Removes and returns the queued stylesheet updates.
    ///
    /// Entries may reference nodes freed since they were queued; the consumer
    /// must revalidate each id (e.g. via [`Self::get`]) before use (L3).
    #[must_use]
    pub fn take_style_updates(&mut self) -> Vec<StyleUpdate> {
        std::mem::take(&mut self.style_updates)
    }

    /// Re-queues a stylesheet update (used when a synchronous style flush
    /// applies the inline `<style>` updates but defers `<link>` loads, which
    /// need the network, back to the page event loop).
    pub fn push_style_update(&mut self, update: StyleUpdate) {
        self.style_updates.push(update);
    }

    /// Removes and returns the queued `<img>` updates (drained by the page to
    /// start image loads).
    ///
    /// As with [`Self::take_style_updates`], an id may name a node freed since
    /// it was queued; revalidate (e.g. via [`Self::get`]) before use (L3).
    #[must_use]
    pub fn take_image_updates(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.image_updates)
    }

    /// Queues an `<img>` element whose `src` became relevant.
    pub fn push_image_update(&mut self, node: NodeId) {
        self.image_updates.push(node);
    }

    // === Custom elements ===

    /// Records that the bindings layer has defined `name`, then enqueues an
    /// [`CustomElementReaction::Upgrade`] for every currently-`Undefined`
    /// element with that local name, in tree order. Returns whether the name
    /// was newly added (it is the bindings layer's job to reject duplicates
    /// before calling this, so a repeat is a no-op).
    pub fn define_custom_element(&mut self, name: String) {
        if !self.defined_names.insert(name.clone()) {
            return;
        }
        // Walk the whole tree in order and upgrade matching undefined elements.
        let doc = self.document;
        let ids: Vec<NodeId> = self.inclusive_descendants(doc).collect();
        for id in ids {
            if self.element_matches_undefined(id, &name) {
                self.custom_reactions
                    .push(CustomElementReaction::Upgrade(id));
            }
        }
    }

    /// Whether `node` is an `Undefined`-state HTML element whose local name is
    /// `name`.
    fn element_matches_undefined(&self, node: NodeId, name: &str) -> bool {
        self.node(node).as_element().is_some_and(|el| {
            el.custom_state == CustomElementState::Undefined
                && el.is_html_element()
                && &*el.name.local == name
        })
    }

    /// Whether the bindings layer has defined a custom element named `name`.
    #[must_use]
    pub fn is_custom_element_defined(&self, name: &str) -> bool {
        self.defined_names.contains(name)
    }

    /// Enqueues a custom-element reaction intent.
    pub fn push_custom_reaction(&mut self, reaction: CustomElementReaction) {
        self.custom_reactions.push(reaction);
    }

    /// Removes and returns the queued custom-element reactions (drained by the
    /// bindings layer at each microtask checkpoint). Entries are `NodeId`
    /// snapshots; revalidate liveness before use (L3).
    #[must_use]
    pub fn take_custom_element_reactions(&mut self) -> Vec<CustomElementReaction> {
        std::mem::take(&mut self.custom_reactions)
    }

    /// The spec's "push a new element queue" (ADR-0021): the current end of the
    /// FIFO, which a `[CEReactions]` operation records on entry and drains back
    /// down to before it returns. A nested operation marks *above* ours, so the
    /// two slices cannot interleave — the Rust call stack is the reactions
    /// stack, and no queue is allocated per call.
    #[must_use]
    pub fn custom_reaction_mark(&self) -> usize {
        self.custom_reactions.len()
    }

    /// Pops the next reaction belonging to the element queue opened at `mark`,
    /// or `None` once the queue has drained back down to it. Reactions below
    /// `mark` belong to an enclosing operation — or, at `mark == 0`, to the
    /// spec's *backup element queue* (the parser's), drained at the microtask
    /// checkpoint. Entries are `NodeId` snapshots; revalidate before use (L3).
    #[must_use]
    pub fn pop_custom_reaction_from(&mut self, mark: usize) -> Option<CustomElementReaction> {
        // `mark` can exceed the length: a reaction that dispatched an event ran
        // a microtask checkpoint, which drains the whole queue.
        (self.custom_reactions.len() > mark).then(|| self.custom_reactions.remove(mark))
    }

    /// Removes and returns queued connectedness changes for pinned (JS-wrapped)
    /// nodes: `(id, connected)`. Drained by the bindings layer to add/drop the
    /// strong wrapper retention that keeps expando properties alive while a node
    /// is connected. Entries are `NodeId` snapshots; revalidate before use (L3).
    #[must_use]
    pub fn take_pinned_connectivity(&mut self) -> Vec<(NodeId, bool)> {
        std::mem::take(&mut self.pinned_connectivity)
    }

    /// The custom-element state of `node`, or `Uncustomized` for non-elements.
    #[must_use]
    pub fn custom_state(&self, node: NodeId) -> CustomElementState {
        self.get(node)
            .and_then(Node::as_element)
            .map_or(CustomElementState::Uncustomized, |el| el.custom_state)
    }

    /// Sets the custom-element state of `node` (called by the bindings layer
    /// after running or failing a constructor). No-op for non-elements.
    pub fn set_custom_state(&mut self, node: NodeId, state: CustomElementState) {
        if let Some(el) = self.arena.get_mut(node).and_then(Node::as_element_mut) {
            el.custom_state = state;
        }
    }

    /// Resets the custom-element registry mirror and reaction queue for a new
    /// navigation. The bindings-side registry is cleared in tandem.
    pub fn clear_custom_elements(&mut self) {
        self.defined_names.clear();
        self.custom_reactions.clear();
        self.pinned_connectivity.clear();
    }

    /// Removes and returns queued `<script>` preparation candidates. Entries
    /// are snapshots and must be revalidated by the consumer.
    #[must_use]
    pub fn take_script_updates(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.script_updates)
    }

    /// The queued `<script>` preparation candidates, without consuming them.
    /// A consumer that runs only some of them claims those with
    /// [`Self::mark_script_already_started`]; the rest stay queued for the
    /// event loop, which skips the claimed ones.
    #[must_use]
    pub fn script_updates(&self) -> &[NodeId] {
        &self.script_updates
    }

    /// Whether a script element has entered preparation/execution already.
    #[must_use]
    pub fn script_already_started(&self, node: NodeId) -> bool {
        self.get(node)
            .and_then(Node::as_element)
            .is_some_and(|el| el.script_already_started)
    }

    /// Atomically marks a script as already started. Returns `true` only to
    /// the first caller; non-script or stale nodes return `false`.
    pub fn mark_script_already_started(&mut self, node: NodeId) -> bool {
        let Some(el) = self.arena.get_mut(node).and_then(Node::as_element_mut) else {
            return false;
        };
        if !el.is_html_element()
            || el.name.local != local_name!("script")
            || el.script_already_started
        {
            return false;
        }
        el.script_already_started = true;
        true
    }

    /// Returns the script element's force-async state.
    #[must_use]
    pub fn script_force_async(&self, node: NodeId) -> bool {
        self.get(node)
            .and_then(Node::as_element)
            .is_some_and(|el| el.script_force_async)
    }

    /// Updates force-async for a script created through DOM APIs or whose
    /// `async` IDL setter has been invoked.
    pub fn set_script_force_async(&mut self, node: NodeId, value: bool) {
        if let Some(el) = self.arena.get_mut(node).and_then(Node::as_element_mut)
            && el.is_html_element()
            && el.name.local == local_name!("script")
        {
            el.script_force_async = value;
        }
    }

    /// Queues an image update for every `<img>` with a `src` in the subtree
    /// rooted at `root` (mirrors [`Self::note_stylesheet_owners`] on connect).
    fn note_image_owners(&mut self, root: NodeId) {
        let ids: Vec<NodeId> = self.inclusive_descendants(root).collect();
        for id in ids {
            if self.node(id).is_image_element() {
                self.push_image_update(id);
            }
        }
    }

    /// Queues every connected HTML `<script>` in a newly connected subtree.
    fn note_script_owners(&mut self, root: NodeId) {
        let ids: Vec<NodeId> = self.inclusive_descendants(root).collect();
        for id in ids {
            if self.node(id).is_connected()
                && self.node(id).as_element().is_some_and(|el| {
                    el.is_html_element() && el.name.local == local_name!("script")
                })
            {
                self.script_updates.push(id);
            }
        }
    }

    /// Enqueues custom-element reactions for a subtree that just became
    /// connected: `Custom` elements get a `connectedCallback`; `Undefined`
    /// elements whose name is now defined get upgraded (the upgrade itself
    /// delivers the initial `connectedCallback`). Tree order.
    fn note_custom_element_connect(&mut self, root: NodeId) {
        let ids: Vec<NodeId> = self.inclusive_descendants(root).collect();
        for id in ids {
            let Some((state, local)) = self
                .node(id)
                .as_element()
                .filter(|el| el.is_html_element())
                .map(|el| (el.custom_state, el.name.local.to_string()))
            else {
                continue;
            };
            match state {
                CustomElementState::Custom => {
                    self.custom_reactions
                        .push(CustomElementReaction::Connected(id));
                }
                CustomElementState::Undefined if self.defined_names.contains(&local) => {
                    self.custom_reactions
                        .push(CustomElementReaction::Upgrade(id));
                }
                _ => {}
            }
        }
    }

    /// Enqueues `disconnectedCallback` reactions for every `Custom` element in
    /// a connected subtree that is about to be removed. Must run *before* the
    /// subtree is detached (while `is_connected` still holds).
    fn note_custom_element_disconnect(&mut self, root: NodeId) {
        let ids: Vec<NodeId> = self.inclusive_descendants(root).collect();
        for id in ids {
            if self.node(id).is_connected()
                && self
                    .node(id)
                    .as_element()
                    .is_some_and(|el| el.custom_state == CustomElementState::Custom)
            {
                self.custom_reactions
                    .push(CustomElementReaction::Disconnected(id));
            }
        }
    }

    /// Called by the parser when a `<style>` element is popped off the open
    /// element stack (its text content is now complete).
    ///
    /// Gated on connectedness like every other stylesheet hook: a `<style>`
    /// parsed into a second document (`DOMParser`) or into template contents
    /// must not queue a `StyleUpdate` against the *page's* style engine. This
    /// is the one hook that used to fire regardless of where the element was.
    pub fn note_style_element_closed(&mut self, node: NodeId) {
        if self.node(node).is_connected() && self.node(node).is_style_element() {
            self.push_style_update(StyleUpdate::StyleElement(node));
        }
    }

    /// Records an element snapshot before an attribute/state mutation, but only
    /// for elements that already carry cascade data (the first cascade styles
    /// everything regardless). Sets the element's `has_snapshot` bit.
    pub(crate) fn snapshot_element(&mut self, element: NodeId) {
        let opaque = opaque_node(element);
        let snapshot = {
            let Some(el) = self.node(element).as_element() else {
                return;
            };
            if !el.stylo.data.has_data() {
                return;
            }
            el.stylo.has_snapshot.set(true);
            el.stylo.snapshot_handled.store(false, Ordering::SeqCst);
            if self.snapshots.contains_key(&opaque) {
                return;
            }
            build_snapshot(el)
        };
        self.snapshots.insert(opaque, snapshot);
    }

    /// The composed parent: the DOM parent, or the host for a shadow root
    /// fragment. Dirty/restyle propagation must cross the shadow boundary or
    /// mutations inside a shadow tree would be invisible to the next restyle
    /// (the chain would die at the parentless fragment).
    fn composed_parent(&self, node: NodeId) -> Option<NodeId> {
        let n = self.node(node);
        if let Some(parent) = n.parent {
            return Some(parent);
        }
        match n.data() {
            NodeData::DocumentFragment {
                host,
                shadow: Some(_),
            } => *host,
            _ => None,
        }
    }

    /// Inserts a conservative restyle hint on the nearest inclusive-ancestor
    /// element that has cascade data, so the next resolution re-matches it.
    /// Walks composed parents so shadow-tree mutations reach the host chain.
    pub(crate) fn note_stylo_restyle(&mut self, node: NodeId) {
        let mut current = Some(node);
        let mut target = None;
        while let Some(id) = current {
            if self
                .node(id)
                .as_element()
                .is_some_and(|el| el.stylo.data.has_data())
            {
                target = Some(id);
                break;
            }
            current = self.composed_parent(id);
        }
        if let Some(id) = target
            && let Some(el) = self.arena.node_mut(id).as_element_mut()
        {
            el.stylo.data.insert_hint(RestyleHint::restyle_subtree());
        }
    }

    /// The document node.
    #[must_use]
    pub fn document(&self) -> NodeId {
        self.document
    }

    /// Number of live nodes in the arena (connected or detached).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.arena.len()
    }

    #[must_use]
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.arena.get(id)
    }

    /// Borrow a known-live node. Panics on a stale id — bindings validate
    /// generations at the boundary and raise a clean error there instead.
    #[must_use]
    pub fn node(&self, id: NodeId) -> &Node {
        self.arena.node(id)
    }

    /// Crate-internal mutable access for parser/serializer bookkeeping that
    /// does not go through the observable mutation path (e.g. the
    /// "script already started" flag).
    pub(crate) fn node_mut_internal(&mut self, id: NodeId) -> &mut Node {
        self.arena.node_mut(id)
    }

    /// The document's mutation observer registry.
    #[must_use]
    pub fn observers(&self) -> &MutationObserverRegistry {
        &self.observers
    }

    /// Mutable access to the observer registry (`new MutationObserver`,
    /// `observe`, `takeRecords`, `disconnect`).
    pub fn observers_mut(&mut self) -> &mut MutationObserverRegistry {
        &mut self.observers
    }

    #[must_use]
    pub fn quirks_mode(&self) -> QuirksMode {
        match self.node(self.document).data() {
            NodeData::Document(doc) => doc.quirks_mode,
            _ => unreachable!("document id always refers to a document node"),
        }
    }

    /// The document's URL (`about:blank` until a load sets it).
    #[must_use]
    pub fn document_url(&self) -> &str {
        match self.node(self.document).data() {
            NodeData::Document(doc) => &doc.url,
            _ => unreachable!("document id always refers to a document node"),
        }
    }

    /// The document base URL: the first `<base href>` in tree order resolved
    /// against the document URL, or the document URL itself when there is none.
    ///
    /// Relative-URL reflection (`a.href`, `img.src`, …) and subresource loading
    /// both resolve against this, not against [`Self::document_url`].
    #[must_use]
    pub fn base_url(&self) -> String {
        let version = self.structure_version.get();
        if let Some((cached_version, base)) = self.base_url_cache.borrow().as_ref()
            && *cached_version == version
        {
            return base.clone();
        }
        let base = self.compute_base_url();
        *self.base_url_cache.borrow_mut() = Some((version, base.clone()));
        base
    }

    fn compute_base_url(&self) -> String {
        self.compute_base_url_of(self.document)
    }

    fn compute_base_url_of(&self, doc: NodeId) -> String {
        let document_url = self.document_url_of(doc);
        let href = self.inclusive_descendants(doc).find_map(|id| {
            let el = self.node(id).as_element()?;
            (el.is_html_element() && el.name.local == local_name!("base"))
                .then(|| el.attr(&crate::node::attr_name(local_name!("href"))))
                .flatten()
        });
        // A `<base>` carrying only `target` does not set the base URL, and an
        // `href` that will not resolve is ignored (HTML "set the frozen base URL").
        let Some(href) = href else {
            return document_url.to_owned();
        };
        url::Url::parse(document_url)
            .and_then(|doc| doc.join(href))
            .map_or_else(|_| document_url.to_owned(), |url| url.to_string())
    }

    pub fn set_document_url(&mut self, url: String) {
        self.url_extra_data = make_url_extra_data(&url);
        // The base URL derives from the document URL, and this mutation does
        // not move `structure_version` — the cache key cannot see it.
        self.base_url_cache.borrow_mut().take();
        if let NodeData::Document(doc) = &mut self.arena.node_mut(self.document).data {
            doc.url = url;
        }
    }

    pub fn set_quirks_mode(&mut self, mode: QuirksMode) {
        self.set_quirks_mode_of(self.document, mode);
    }

    // === Node documents (spec "node document" / `ownerDocument`) ===

    /// Spec `ownerDocument`: the node's node document, or `None` iff `id` *is*
    /// a Document.
    #[must_use]
    pub fn owner_document(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).owner
    }

    /// The node document of `id` — `id` itself when it is a Document. This is
    /// the "node document" every spec algorithm means.
    #[must_use]
    pub fn node_document(&self, id: NodeId) -> NodeId {
        self.node(id).owner.unwrap_or(id)
    }

    /// Per-document payload. `None` if `doc` is not a Document node.
    #[must_use]
    pub fn document_data(&self, doc: NodeId) -> Option<&DocumentData> {
        match self.node(doc).data() {
            NodeData::Document(data) => Some(data),
            _ => None,
        }
    }

    pub fn document_data_mut(&mut self, doc: NodeId) -> Option<&mut DocumentData> {
        match &mut self.arena.node_mut(doc).data {
            NodeData::Document(data) => Some(data),
            _ => None,
        }
    }

    #[must_use]
    pub fn quirks_mode_of(&self, doc: NodeId) -> QuirksMode {
        self.document_data(doc)
            .map_or(QuirksMode::NoQuirks, |d| d.quirks_mode)
    }

    pub fn set_quirks_mode_of(&mut self, doc: NodeId, mode: QuirksMode) {
        if let Some(data) = self.document_data_mut(doc) {
            data.quirks_mode = mode;
        }
    }

    #[must_use]
    pub fn document_url_of(&self, doc: NodeId) -> &str {
        self.document_data(doc).map_or("about:blank", |d| &d.url)
    }

    pub fn set_document_url_of(&mut self, doc: NodeId, url: String) {
        if doc == self.document {
            self.set_document_url(url);
        } else if let Some(data) = self.document_data_mut(doc) {
            data.url = url;
        }
    }

    /// Whether `doc` is an HTML document (drives `createElement` lowercasing,
    /// the namespace it assigns, and `createCDATASection`).
    #[must_use]
    pub fn is_html_document(&self, doc: NodeId) -> bool {
        self.document_data(doc).is_some_and(DocumentData::is_html)
    }

    /// The document element (first element child) of `doc`.
    #[must_use]
    pub fn document_element_of(&self, doc: NodeId) -> Option<NodeId> {
        self.children(doc)
            .find(|&c| self.node(c).data().kind() == NodeKind::Element)
    }

    /// The base URL of `doc`. Only the page document's is memoized — the cache
    /// is keyed on the tree-wide `structure_version` and would otherwise serve
    /// one document's base URL to another.
    #[must_use]
    pub fn base_url_of(&self, doc: NodeId) -> String {
        if doc == self.document {
            return self.base_url();
        }
        self.compute_base_url_of(doc)
    }

    /// Spec connectedness: `id`'s shadow-including root is a Document — true
    /// even inside a `new Document()`, which has no browsing context.
    ///
    /// Deliberately *not* the same predicate as [`NodeFlags::IS_CONNECTED`],
    /// which keeps its narrower engine meaning of "in the rendered document"
    /// and gates style, layout, resource loading, custom-element upgrades, the
    /// `getElementById` index, and event bubbling to the `Window`. Only the JS
    /// `Node.isConnected` getter uses this.
    #[must_use]
    pub fn is_spec_connected(&self, id: NodeId) -> bool {
        // O(1) for the page tree, which is every hot caller.
        if self.node(id).is_connected() {
            return true;
        }
        let mut current = id;
        // `composed_parent` crosses a shadow root to its host but *not*
        // template contents to theirs — which is precisely the spec's
        // shadow-including root, and keeps `template.content` disconnected.
        while let Some(parent) = self.composed_parent(current) {
            current = parent;
        }
        self.node(current).data().kind() == NodeKind::Document
    }

    // === Adoption (spec "adopt a node") ===

    /// Spec `adopt`: removes `node` from its parent and re-owns the composed
    /// subtree to `document`.
    ///
    /// `adoptedCallback` is not implemented (see ADR-0017); nothing else in the
    /// algorithm is skipped.
    pub fn adopt(&mut self, node: NodeId, document: NodeId) {
        if self.node(node).parent.is_some() {
            self.remove_internal(node, false);
        }
        self.set_owner_composed(node, document);
    }

    /// Re-owns the composed subtree rooted at `root` (children, template
    /// contents, and shadow trees) to `owner`, moving each node's owner pin
    /// with it.
    fn set_owner_composed(&mut self, root: NodeId, owner: NodeId) {
        // Owners are uniform across a composed subtree — every path that
        // creates or inserts a node keeps them so — which makes this early-out
        // exact rather than a heuristic, and keeps the common insert O(1).
        if self.node(root).owner == Some(owner) {
            return;
        }
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            stack.extend(self.children(id));
            if let Some(el) = self.node(id).as_element() {
                if let Some(contents) = el.template_contents {
                    stack.push(contents);
                }
                if let Some(shadow) = el.shadow_root {
                    stack.push(shadow);
                }
            }
            let previous = self.arena.node(id).owner;
            if previous == Some(owner) {
                continue;
            }
            self.arena.node_mut(id).owner = Some(owner);
            // The node's pins were counted against its old node document; move
            // them, or the old document could be freed while this node's
            // `ownerDocument` still named it (or be kept alive forever).
            let held = self.pins.get(&id).copied().unwrap_or(0);
            for _ in 0..held {
                if let Some(previous) = previous {
                    self.release_pin(previous);
                }
                self.add_pin(owner);
            }
        }
    }

    // === Node creation (all nodes start detached) ===
    //
    // Each `create_*` creates in the page document — that is what every spec
    // constructor whose text says "the current global object's associated
    // document" (`new Text()`, `new Comment()`, `new DocumentFragment()`) and
    // every internal caller wants. The `_in` variants take an explicit node
    // document and back `Document.create*`, the parser sink, and cloning.

    /// Creates a second, inert Document.
    ///
    /// It is *structurally* inert rather than defensively so: `IS_CONNECTED` is
    /// never set (`propagate_connectedness` grants it only under
    /// `self.document`), so style, layout, and every resource hook skip it; and
    /// a Document can never acquire a parent (the hierarchy check rejects it),
    /// so it stays a detached root for life. It is freed like a fragment, once
    /// the last JS wrapper in it is collected.
    pub fn create_document(&mut self, data: DocumentData) -> NodeId {
        self.arena.alloc(Node::new(NodeData::Document(data)))
    }

    pub fn create_element(&mut self, name: QualName, attrs: Vec<Attribute>) -> NodeId {
        self.create_element_in(self.document, name, attrs)
    }

    pub fn create_element_in(
        &mut self,
        owner: NodeId,
        name: QualName,
        attrs: Vec<Attribute>,
    ) -> NodeId {
        self.create_element_in_inner(owner, name, attrs, true)
    }

    /// `create_element_in` with control over the "try to upgrade" step.
    ///
    /// `try_upgrade == false` is the fragment-parse path ([`Self::copy_node_from`]):
    /// the spec parses a fragment in a browsing-context-less temp document,
    /// whose registry is empty, so *creation* never upgrades. Upgrading falls to
    /// the insertion steps, which run only when the parent is connected — that
    /// is why `host.innerHTML = '<x-el>'` on a **detached** host leaves the
    /// element `Undefined` (ADR-0021 §6).
    fn create_element_in_inner(
        &mut self,
        owner: NodeId,
        name: QualName,
        attrs: Vec<Attribute>,
        try_upgrade: bool,
    ) -> NodeId {
        // Autonomous custom elements: an HTML element whose local name is a
        // valid custom element name starts `Undefined`. If its name is already
        // defined, it is enqueued for upgrade right away (createElement and the
        // parser both funnel through here).
        let is_potential_custom = name.ns == ns!(html) && is_valid_custom_element_name(&name.local);
        let mut data = ElementData::new(name);
        data.attrs = attrs;
        if is_potential_custom {
            data.custom_state = CustomElementState::Undefined;
        }
        data.refresh_selector_caches(&self.style_lock, &self.url_extra_data);
        let node = self
            .arena
            .alloc(Node::new_in(NodeData::Element(Box::new(data)), owner));
        // Only the page document has a browsing context, so only there does
        // "look up a custom element definition" find one. Upgrading an element
        // of a second document would run a constructor and strand a strong
        // wrapper in `PageState::custom_wrappers` keyed by a node that document
        // outlives.
        if try_upgrade && is_potential_custom && owner == self.document {
            let local = self
                .node(node)
                .as_element()
                .map(|el| el.name.local.to_string());
            if let Some(local) = local
                && self.defined_names.contains(&local)
            {
                self.custom_reactions
                    .push(CustomElementReaction::Upgrade(node));
            }
        }
        // Derive the element-state bits (`:enabled`, `:checked` from a `checked`
        // content attribute, …) up front, so `el.matches(":enabled")` is right
        // on an element that is still detached.
        self.update_element_state(node);
        node
    }

    pub fn create_text(&mut self, text: StrTendril) -> NodeId {
        self.create_text_in(self.document, text)
    }

    pub fn create_text_in(&mut self, owner: NodeId, text: StrTendril) -> NodeId {
        self.arena.alloc(Node::new_in(NodeData::Text(text), owner))
    }

    pub fn create_cdata_section(&mut self, data: StrTendril) -> NodeId {
        self.create_cdata_section_in(self.document, data)
    }

    pub fn create_cdata_section_in(&mut self, owner: NodeId, data: StrTendril) -> NodeId {
        self.arena
            .alloc(Node::new_in(NodeData::CdataSection(data), owner))
    }

    pub fn create_comment(&mut self, text: StrTendril) -> NodeId {
        self.create_comment_in(self.document, text)
    }

    pub fn create_comment_in(&mut self, owner: NodeId, text: StrTendril) -> NodeId {
        self.arena
            .alloc(Node::new_in(NodeData::Comment(text), owner))
    }

    pub fn create_processing_instruction(
        &mut self,
        target: StrTendril,
        data: StrTendril,
    ) -> NodeId {
        self.create_processing_instruction_in(self.document, target, data)
    }

    pub fn create_processing_instruction_in(
        &mut self,
        owner: NodeId,
        target: StrTendril,
        data: StrTendril,
    ) -> NodeId {
        self.arena.alloc(Node::new_in(
            NodeData::ProcessingInstruction { target, data },
            owner,
        ))
    }

    pub fn create_doctype(
        &mut self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) -> NodeId {
        self.create_doctype_in(self.document, name, public_id, system_id)
    }

    pub fn create_doctype_in(
        &mut self,
        owner: NodeId,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) -> NodeId {
        self.arena.alloc(Node::new_in(
            NodeData::Doctype {
                name,
                public_id,
                system_id,
            },
            owner,
        ))
    }

    pub fn create_document_fragment(&mut self) -> NodeId {
        self.create_document_fragment_in(self.document)
    }

    pub fn create_document_fragment_in(&mut self, owner: NodeId) -> NodeId {
        self.arena.alloc(Node::new_in(
            NodeData::DocumentFragment {
                host: None,
                shadow: None,
            },
            owner,
        ))
    }

    /// Creates the template-contents fragment for `host` and links it.
    pub(crate) fn create_template_contents(&mut self, host: NodeId) -> NodeId {
        let owner = self.node_document(host);
        let fragment = self.arena.alloc(Node::new_in(
            NodeData::DocumentFragment {
                host: Some(host),
                shadow: None,
            },
            owner,
        ));
        if let Some(el) = self.arena.node_mut(host).as_element_mut() {
            el.template_contents = Some(fragment);
        }
        fragment
    }

    // === Shadow DOM (DOM spec §4.8) ===

    /// Spec `attachShadow`: creates and links a shadow root fragment to
    /// `host`. Validates that `host` is an HTML element whose name may host a
    /// shadow root (`NotSupportedError`) and does not already have one
    /// (`InvalidStateError`).
    pub fn attach_shadow(
        &mut self,
        host: NodeId,
        mode: ShadowMode,
    ) -> Result<NodeId, DomException> {
        {
            let Some(el) = self.node(host).as_element() else {
                return Err(DomException::new(
                    DomExceptionKind::NotSupportedError,
                    "attachShadow host must be an element",
                ));
            };
            if !el.is_html_element() || !is_valid_shadow_host_name(&el.name.local) {
                return Err(DomException::new(
                    DomExceptionKind::NotSupportedError,
                    "element does not support attachShadow",
                ));
            }
            if el.shadow_root.is_some() {
                return Err(DomException::new(
                    DomExceptionKind::InvalidStateError,
                    "element already hosts a shadow root",
                ));
            }
        }
        let fragment = self.arena.alloc(Node::new(NodeData::DocumentFragment {
            host: Some(host),
            shadow: Some(mode),
        }));
        if let Some(el) = self.arena.node_mut(host).as_element_mut() {
            el.shadow_root = Some(fragment);
        }
        self.shadow_roots.insert(fragment);
        // The shadow tree participates in connectedness from the host.
        let connected = self.node(host).is_connected();
        self.set_connectedness_composed(fragment, connected);
        // Attaching a shadow root changes the host's flat-tree children:
        // funnel through the invalidation hook so structure_version moves and
        // dirty bits/restyle hints propagate.
        self.note_children_changed(host);
        self.note_slot_assignment_changed(host);
        Ok(fragment)
    }

    /// Slot assignment for `host`'s light children (may have) changed:
    /// posts a subtree restyle hint on every already-styled slottable so the
    /// next traversal re-matches it under its new flat-tree position.
    ///
    /// This mirrors Gecko's explicit slot-assignment invalidation. It cannot
    /// be left to ordinary hint propagation: stylo propagates hints only
    /// through elements that already carry cascade data, and a freshly
    /// created `<slot>` (e.g. via `shadowRoot.innerHTML`) has none — the
    /// hint chain from the host would die at it and its assigned nodes would
    /// keep stale styles.
    fn note_slot_assignment_changed(&mut self, host: NodeId) {
        let children: Vec<NodeId> = self.children(host).collect();
        for child in children {
            if let Some(el) = self.arena.node_mut(child).as_element_mut()
                && el.stylo.data.has_data()
            {
                el.stylo.data.insert_hint(RestyleHint::restyle_subtree());
            }
        }
        if let Some(el) = self.arena.node_mut(host).as_element_mut() {
            el.stylo.dirty_descendants.set(true);
        }
        self.mark_dirty_ancestors(host);
    }

    /// Whether the subtree rooted at `node` contains an HTML `<slot>`.
    fn subtree_contains_slot(&self, node: NodeId) -> bool {
        self.inclusive_descendants(node)
            .any(|id| self.is_slot_element(id))
    }

    /// A subtree was inserted into or removed from a shadow tree, or a
    /// slot-relevant attribute changed: re-hint the affected host's
    /// slottables when the mutation can change slot assignment.
    fn note_shadow_mutation(&mut self, node: NodeId) {
        let Some(root) = self.containing_shadow_root(node) else {
            return;
        };
        if !self.subtree_contains_slot(node) {
            return;
        }
        if let Some(host) = self.shadow_host(root) {
            self.note_slot_assignment_changed(host);
        }
    }

    /// The shadow root attached to `host`, regardless of mode.
    #[must_use]
    pub fn shadow_root(&self, host: NodeId) -> Option<NodeId> {
        self.get(host).and_then(Node::as_element)?.shadow_root()
    }

    /// The host element of `fragment`, if it is a shadow root.
    #[must_use]
    pub fn shadow_host(&self, fragment: NodeId) -> Option<NodeId> {
        match self.get(fragment)?.data() {
            NodeData::DocumentFragment {
                host: Some(host),
                shadow: Some(_),
            } => Some(*host),
            _ => None,
        }
    }

    /// The mode of `fragment`, if it is a shadow root.
    #[must_use]
    pub fn shadow_mode(&self, fragment: NodeId) -> Option<ShadowMode> {
        match self.get(fragment)?.data() {
            NodeData::DocumentFragment {
                shadow: Some(mode), ..
            } => Some(*mode),
            _ => None,
        }
    }

    /// Whether `node` is a shadow root fragment.
    #[must_use]
    pub fn is_shadow_root(&self, node: NodeId) -> bool {
        self.shadow_mode(node).is_some()
    }

    /// Whether any live shadow roots exist in the tree. Layout uses this to
    /// bail out of incremental patching (shadow mutations are invisible to
    /// the light-tree snapshot walk).
    #[must_use]
    pub fn has_shadow_roots(&self) -> bool {
        !self.shadow_roots.is_empty()
    }

    /// A scope's `adoptedStyleSheets` changed: funnels through the
    /// invalidation hook at the scope's attachment point (shadow host, or the
    /// document node itself) so restyle hints and version counters move.
    pub fn note_adopted_sheets_changed(&mut self, node: NodeId) {
        let target = self.shadow_host(node).unwrap_or(node);
        self.note_children_changed(target);
    }

    /// The shadow root containing `node`: its tree root when that root is a
    /// shadow root fragment (the fragment itself is "contained" too).
    #[must_use]
    pub fn containing_shadow_root(&self, node: NodeId) -> Option<NodeId> {
        let mut current = node;
        while let Some(parent) = self.node(current).parent {
            current = parent;
        }
        self.is_shadow_root(current).then_some(current)
    }

    /// Whether `node` is an HTML `<slot>` element.
    #[must_use]
    pub fn is_slot_element(&self, node: NodeId) -> bool {
        self.get(node)
            .and_then(Node::as_element)
            .is_some_and(|el| el.is_html_element() && el.name.local == local_name!("slot"))
    }

    /// The slot name a slottable (element or text) node would be assigned
    /// under: an element's `slot` attribute (default empty), the empty name
    /// for text nodes (they carry no attributes), `None` for non-slottables.
    fn slottable_name(&self, node: NodeId) -> Option<String> {
        match self.node(node).data() {
            NodeData::Element(el) => Some(
                el.attr(&crate::node::attr_name(local_name!("slot")))
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
            ),
            NodeData::Text(_) | NodeData::CdataSection(_) => Some(String::new()),
            _ => None,
        }
    }

    /// The `name` attribute of a `<slot>` element (default empty).
    fn slot_name(&self, slot: NodeId) -> String {
        self.node(slot)
            .as_element()
            .and_then(|el| el.attr(&crate::node::attr_name(local_name!("name"))))
            .map(|v| v.to_string())
            .unwrap_or_default()
    }

    /// Spec "find a slot": the first `<slot>` in `shadow_root`'s tree, in
    /// tree order, whose name is `name`.
    ///
    /// The name → slot map is cached per shadow root and keyed by
    /// `structure_version` (child-list changes and `name` attribute writes
    /// both move it): `assigned_slot`/`flat_tree_parent` run per slottable in
    /// the restyle hot path, and rescanning the shadow tree each time would
    /// be O(slottables × shadow size).
    fn find_slot(&self, shadow_root: NodeId, name: &str) -> Option<NodeId> {
        let version = self.structure_version.get();
        {
            let cache = self.slot_cache.borrow();
            if let Some((cached_version, map)) = cache.get(&shadow_root)
                && *cached_version == version
            {
                return map.get(name).copied();
            }
        }
        let mut map: HashMap<String, NodeId> = HashMap::new();
        for id in self.inclusive_descendants(shadow_root) {
            if self.is_slot_element(id) {
                // First slot of a given name wins (tree order).
                map.entry(self.slot_name(id)).or_insert(id);
            }
        }
        let result = map.get(name).copied();
        self.slot_cache
            .borrow_mut()
            .insert(shadow_root, (version, map));
        result
    }

    /// Spec `assignedSlot`: the slot in the parent's shadow root that `node`
    /// is assigned to, if any.
    #[must_use]
    pub fn assigned_slot(&self, node: NodeId) -> Option<NodeId> {
        let parent = self.node(node).parent?;
        let shadow = self.node(parent).as_element()?.shadow_root()?;
        let name = self.slottable_name(node)?;
        self.find_slot(shadow, &name)
    }

    /// The light-tree nodes assigned to `slot`, in tree order (spec
    /// `assignedNodes()` without `flatten`). Empty when `slot` is not in a
    /// shadow tree or a same-named slot precedes it.
    #[must_use]
    pub fn assigned_slot_nodes(&self, slot: NodeId) -> Vec<NodeId> {
        let Some(shadow_root) = self.containing_shadow_root(slot) else {
            return Vec::new();
        };
        let Some(host) = self.shadow_host(shadow_root) else {
            return Vec::new();
        };
        let name = self.slot_name(slot);
        if self.find_slot(shadow_root, &name) != Some(slot) {
            return Vec::new();
        }
        self.children(host)
            .filter(|&child| self.slottable_name(child).is_some_and(|n| n == name))
            .collect()
    }

    /// Flat-tree children of `node` (CSS scoping "flattened tree"), the
    /// child list layout and style traversal walk:
    /// - a shadow host's flat children are its shadow root's children;
    /// - a `<slot>`'s flat children are its assigned nodes, or its own
    ///   children as fallback content when nothing is assigned;
    /// - anything else keeps its ordinary children (unassigned light
    ///   children of a host thereby vanish from the flat tree).
    #[must_use]
    pub fn flat_tree_children(&self, node: NodeId) -> Vec<NodeId> {
        if let Some(el) = self.node(node).as_element() {
            if let Some(shadow) = el.shadow_root() {
                return self.children(shadow).collect();
            }
            if self.is_slot_element(node) {
                let assigned = self.assigned_slot_nodes(node);
                if !assigned.is_empty() {
                    return assigned;
                }
            }
        }
        self.children(node).collect()
    }

    /// Flat-tree parent of `node`: its assigned slot when it is assigned,
    /// its host when it is a shadow root fragment, else its DOM parent.
    #[must_use]
    pub fn flat_tree_parent(&self, node: NodeId) -> Option<NodeId> {
        if let Some(slot) = self.assigned_slot(node) {
            return Some(slot);
        }
        if let Some(parent) = self.node(node).parent {
            // An unassigned light child of a shadow host has no flat parent.
            if self
                .node(parent)
                .as_element()
                .is_some_and(|el| el.shadow_root().is_some())
            {
                return None;
            }
            return Some(parent);
        }
        self.shadow_host(node)
    }

    // === Traversal ===

    /// Children of `id`, in tree order.
    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut next = self.node(id).first_child;
        std::iter::from_fn(move || {
            let current = next?;
            next = self.node(current).next_sibling;
            Some(current)
        })
    }

    /// Ancestors of `id`, closest first, excluding `id`.
    pub fn ancestors(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut next = self.node(id).parent;
        std::iter::from_fn(move || {
            let current = next?;
            next = self.node(current).parent;
            Some(current)
        })
    }

    /// `id` followed by its ancestors, closest first.
    pub fn inclusive_ancestors(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::once(id).chain(self.ancestors(id))
    }

    /// Descendants of `root` in tree order, including `root`.
    pub fn inclusive_descendants(&self, root: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut next = Some(root);
        std::iter::from_fn(move || {
            let current = next?;
            next = self.following_in_subtree(current, root);
            Some(current)
        })
    }

    /// The node after `id` in tree order, without leaving `root`'s subtree.
    fn following_in_subtree(&self, id: NodeId, root: NodeId) -> Option<NodeId> {
        let node = self.node(id);
        if let Some(child) = node.first_child {
            return Some(child);
        }
        let mut current = id;
        loop {
            if current == root {
                return None;
            }
            let n = self.node(current);
            if let Some(sibling) = n.next_sibling {
                return Some(sibling);
            }
            current = n.parent?;
        }
    }

    /// Spec "host-including inclusive ancestor": follows fragment→host links
    /// (template contents) in addition to parent links.
    fn is_host_including_inclusive_ancestor(&self, a: NodeId, b: NodeId) -> bool {
        let mut current = Some(b);
        while let Some(id) = current {
            if id == a {
                return true;
            }
            let node = self.node(id);
            current = node.parent.or(match node.data() {
                NodeData::DocumentFragment { host, .. } => *host,
                _ => None,
            });
        }
        false
    }

    /// The document element (first element child of the document), if any.
    #[must_use]
    pub fn document_element(&self) -> Option<NodeId> {
        self.children(self.document)
            .find(|&c| self.node(c).data().kind() == NodeKind::Element)
    }

    /// Concatenation of the text descendants of `id` (spec `textContent`
    /// for element/fragment roots).
    #[must_use]
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        for n in self.inclusive_descendants(id) {
            let node = self.node(n);
            if node.is_text()
                && let Some(t) = node.character_data()
            {
                out.push_str(t);
            }
        }
        out
    }

    /// True if `id` is a Text node consisting solely of ASCII whitespace.
    /// Layout ignores such nodes when classifying block vs. inline children
    /// (mirrors blitz-dom's `is_whitespace_node`).
    #[must_use]
    pub fn is_whitespace_text(&self, id: NodeId) -> bool {
        match self.node(id).data() {
            NodeData::Text(t) | NodeData::CdataSection(t) => {
                t.chars().all(|c| c.is_ascii_whitespace())
            }
            _ => false,
        }
    }

    /// The element's primary computed style, if the cascade has produced one.
    #[must_use]
    pub fn primary_style(
        &self,
        id: NodeId,
    ) -> Option<servo_arc::Arc<style::properties::ComputedValues>> {
        let el = self.node(id).as_element()?;
        el.stylo.data.primary_styles().map(|s| (*s).clone())
    }

    /// The element's computed style for `pseudo` (`::before`/`::after`), if
    /// the cascade produced one.
    #[must_use]
    pub fn pseudo_style(
        &self,
        id: NodeId,
        pseudo: &style::selector_parser::PseudoElement,
    ) -> Option<servo_arc::Arc<style::properties::ComputedValues>> {
        let el = self.node(id).as_element()?;
        let data = el.stylo.data.get()?;
        data.styles.pseudos.get(pseudo).cloned()
    }

    // === Spec mutation algorithms (validated public API) ===

    /// Spec `pre-insert`: insert `node` into `parent` before `child`.
    pub fn insert_before(
        &mut self,
        parent: NodeId,
        node: NodeId,
        child: Option<NodeId>,
    ) -> Result<NodeId, DomException> {
        self.ensure_pre_insert_validity(node, parent, child)?;
        let reference_child = match child {
            Some(c) if c == node => self.node(node).next_sibling,
            other => other,
        };
        self.insert_internal(node, parent, reference_child, false);
        Ok(node)
    }

    /// Spec `append`: insert `node` as the last child of `parent`.
    pub fn append_child(&mut self, parent: NodeId, node: NodeId) -> Result<NodeId, DomException> {
        self.insert_before(parent, node, None)
    }

    /// Spec `replace`: replace `child` with `node` within `parent`.
    pub fn replace_child(
        &mut self,
        parent: NodeId,
        node: NodeId,
        child: NodeId,
    ) -> Result<NodeId, DomException> {
        self.ensure_replace_validity(node, parent, child)?;

        let reference_child = match self.node(child).next_sibling {
            Some(c) if c == node => self.node(node).next_sibling,
            other => other,
        };
        let previous_sibling = self.node(child).prev_sibling;
        let mut removed_nodes = Vec::new();
        if self.node(child).parent.is_some() {
            removed_nodes.push(child);
            self.remove_internal(child, true);
        }
        let added_nodes = match self.node(node).data() {
            NodeData::DocumentFragment { .. } => self.children(node).collect(),
            _ => vec![node],
        };
        self.insert_internal(node, parent, reference_child, true);
        self.queue_child_list_record(
            parent,
            RecordContents {
                added_nodes,
                removed_nodes,
                previous_sibling,
                next_sibling: reference_child,
            },
        );
        Ok(child)
    }

    /// Spec `pre-remove`: remove `child` from `parent`.
    pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Result<NodeId, DomException> {
        if self.node(child).parent != Some(parent) {
            return Err(DomException::new(
                DomExceptionKind::NotFoundError,
                "node to be removed is not a child of this node",
            ));
        }
        self.remove_internal(child, false);
        Ok(child)
    }

    /// Removes `node` from its parent, if it has one (spec `remove` on a
    /// free-standing node; used by `Element.remove()`).
    pub fn remove(&mut self, node: NodeId) {
        if self.node(node).parent.is_some() {
            self.remove_internal(node, false);
        }
    }

    /// Spec "replace all with node within parent": removes every child of
    /// `parent` and inserts `node` (if any), queueing **exactly one** childList
    /// record naming all the removals and the addition together.
    ///
    /// The removals and the insertion each run with the suppress-observers flag
    /// set, which is the whole point: doing this as `remove()`-per-child plus
    /// `append_child` queues N+1 records, and `textContent =` is specified to
    /// produce one. Returns the removed children so the caller can free them.
    pub fn replace_all(&mut self, parent: NodeId, node: Option<NodeId>) -> Vec<NodeId> {
        let removed: Vec<NodeId> = self.children(parent).collect();
        for &child in &removed {
            self.remove_internal(child, true);
        }
        if let Some(node) = node {
            self.insert_internal(node, parent, None, true);
        }
        if !removed.is_empty() || node.is_some() {
            self.queue_child_list_record(
                parent,
                RecordContents {
                    added_nodes: node.into_iter().collect(),
                    removed_nodes: removed.clone(),
                    previous_sibling: None,
                    next_sibling: None,
                },
            );
        }
        removed
    }

    /// Frees a **detached** subtree, invalidating every id inside it.
    ///
    /// Returns an `InvalidStateError` if `node` still has a parent, is the
    /// document, or the subtree contains pinned nodes (live JS wrappers).
    /// Also frees template-contents fragments hanging off the subtree.
    pub fn free_subtree(&mut self, node: NodeId) -> Result<(), DomException> {
        if node == self.document || self.node(node).parent.is_some() {
            return Err(DomException::new(
                DomExceptionKind::InvalidStateError,
                "only detached non-document subtrees can be freed",
            ));
        }
        if self.subtree_has_pins(node) {
            return Err(DomException::new(
                DomExceptionKind::InvalidStateError,
                "subtree contains pinned nodes (live JS wrappers)",
            ));
        }
        let mut stack = vec![node];
        while let Some(id) = stack.pop() {
            stack.extend(self.children(id));
            if let Some(el) = self.node(id).as_element() {
                if let Some(contents) = el.template_contents {
                    stack.push(contents);
                }
                if let Some(shadow) = el.shadow_root {
                    stack.push(shadow);
                }
            }
            if self.is_shadow_root(id) {
                self.shadow_roots.remove(&id);
                self.shadow_cascade.remove(&id);
                self.slot_cache.borrow_mut().remove(&id);
            }
            self.listeners.remove_node(id);
            self.observers.remove_node(id);
            self.arena.free(id);
        }
        Ok(())
    }

    // === JS wrapper pins (design doc §5.3) ===
    //
    // A live JS wrapper pins its node. Detaching a subtree with no pinned
    // nodes frees it immediately; pinned detached trees survive until their
    // wrappers are GC'd. The bindings drive both transitions through
    // [`Self::free_detached_tree_if_unpinned`].
    //
    // **A pinned node also pins its node document.** Spec-wise a node keeps its
    // `ownerDocument` alive, and the engine needs it literally: a node created
    // by `doc2.createElement()` and never inserted is its *own* detached root,
    // so it does not appear in doc2's subtree and `subtree_has_pins(doc2)`
    // cannot see it. Without the owner pin, GC of the doc2 wrapper would free
    // doc2 while that element is still live, leaving `el.ownerDocument` naming
    // a freed slot. So `pins[doc]` counts doc's own wrappers *plus* one per
    // pinned node it owns — freeing only ever asks whether that total is zero,
    // which is exactly "is this document still referenced".

    /// Increments `node`'s pin count, and its node document's with it.
    pub fn pin(&mut self, node: NodeId) {
        debug_assert!(self.get(node).is_some(), "pinning a freed node");
        self.add_pin(node);
        if let Some(owner) = self.node(node).owner {
            self.add_pin(owner);
        }
    }

    /// Decrements `node`'s pin count, and its node document's with it.
    pub fn unpin(&mut self, node: NodeId) {
        // A stale id is a no-op: navigation drops the whole arena, pins map
        // included, and a live node can never be freed while pinned.
        let owner = self.get(node).and_then(Node::owner);
        self.release_pin(node);
        if let Some(owner) = owner {
            self.release_pin(owner);
        }
    }

    fn add_pin(&mut self, node: NodeId) {
        *self.pins.entry(node).or_insert(0) += 1;
    }

    fn release_pin(&mut self, node: NodeId) {
        if let Some(count) = self.pins.get_mut(&node) {
            *count -= 1;
            if *count == 0 {
                self.pins.remove(&node);
            }
        }
    }

    /// True if any node of the subtree rooted at `root` (including template
    /// contents) is pinned.
    #[must_use]
    pub fn subtree_has_pins(&self, root: NodeId) -> bool {
        if self.pins.is_empty() {
            return false;
        }
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if self.pins.contains_key(&id) {
                return true;
            }
            stack.extend(self.children(id));
            if let Some(el) = self.node(id).as_element() {
                if let Some(contents) = el.template_contents {
                    stack.push(contents);
                }
                if let Some(shadow) = el.shadow_root {
                    stack.push(shadow);
                }
            }
        }
        false
    }

    /// The root of the tree containing `node`, following template-contents
    /// fragment → host links (a template's contents live and die with the
    /// tree its host belongs to).
    #[must_use]
    pub fn tree_root_via_host(&self, node: NodeId) -> NodeId {
        let mut current = node;
        loop {
            let n = self.node(current);
            if let Some(parent) = n.parent {
                current = parent;
                continue;
            }
            if let NodeData::DocumentFragment {
                host: Some(host), ..
            } = n.data()
            {
                current = *host;
                continue;
            }
            return current;
        }
    }

    /// Frees the detached tree containing `node` if the whole tree is
    /// unpinned. Returns whether it was freed. Never frees the document
    /// tree. A stale (already freed) id is a no-op.
    pub fn free_detached_tree_if_unpinned(&mut self, node: NodeId) -> bool {
        if self.get(node).is_none() {
            return false;
        }
        let root = self.tree_root_via_host(node);
        if root == self.document {
            return false;
        }
        self.free_subtree(root).is_ok()
    }

    // === Cloning (spec "clone a node") ===

    /// Returns a detached copy of `node`; with `deep`, clones the whole
    /// subtree. `<template>` contents are cloned only on deep clones, per
    /// the HTML cloning steps. Cloning a Document is not supported.
    ///
    /// The clone stays in `node`'s own node document — `cloneNode` never
    /// changes documents. [`Self::clone_subtree_into`] backs `importNode`.
    pub fn clone_subtree(&mut self, node: NodeId, deep: bool) -> Result<NodeId, DomException> {
        let owner = self.node_document(node);
        self.clone_subtree_into(node, deep, owner)
    }

    /// Spec "clone a node" with an explicit target document (`importNode`).
    pub fn clone_subtree_into(
        &mut self,
        node: NodeId,
        deep: bool,
        owner: NodeId,
    ) -> Result<NodeId, DomException> {
        if self.node(node).data().kind() == NodeKind::Document {
            return Err(DomException::new(
                DomExceptionKind::NotSupportedError,
                "cloning a Document is not supported",
            ));
        }
        Ok(self.clone_node_inner(node, deep, owner))
    }

    fn clone_node_inner(&mut self, node: NodeId, deep: bool, owner: NodeId) -> NodeId {
        // Explicit-stack traversal so a deeply nested subtree cannot overflow
        // the native stack. Each pending task copies `source`'s children under
        // the already-created `target` clone.
        let mut stack: Vec<(NodeId, NodeId)> = Vec::new();
        let copy = self.clone_shallow(node, deep, owner, &mut stack);
        while let Some((source, target)) = stack.pop() {
            let children: Vec<NodeId> = self.children(source).collect();
            for child in children {
                let child_copy = self.clone_shallow(child, true, owner, &mut stack);
                self.insert_internal(child_copy, target, None, true);
            }
        }
        copy
    }

    /// Creates a shallow copy of `node` owned by `owner`. When `deep`, a task
    /// to copy `node`'s children is pushed onto `stack`; for a `<template>`
    /// element a task to copy its (separate-tree) template contents is pushed
    /// as well.
    fn clone_shallow(
        &mut self,
        node: NodeId,
        deep: bool,
        owner: NodeId,
        stack: &mut Vec<(NodeId, NodeId)>,
    ) -> NodeId {
        let copy = match self.node(node).data() {
            NodeData::Element(el) => {
                let name = el.name.clone();
                let attrs = el.attrs.clone();
                let contents = el.template_contents;
                let copy = self.create_element_in(owner, name, attrs);
                if let Some(contents) = contents {
                    let contents_copy = self.create_template_contents(copy);
                    if deep {
                        stack.push((contents, contents_copy));
                    }
                }
                copy
            }
            NodeData::Text(t) => {
                let t = t.clone();
                self.create_text_in(owner, t)
            }
            NodeData::CdataSection(t) => {
                let t = t.clone();
                self.create_cdata_section_in(owner, t)
            }
            NodeData::Comment(t) => {
                let t = t.clone();
                self.create_comment_in(owner, t)
            }
            NodeData::ProcessingInstruction { target, data } => {
                let (target, data) = (target.clone(), data.clone());
                self.create_processing_instruction_in(owner, target, data)
            }
            NodeData::Doctype {
                name,
                public_id,
                system_id,
            } => {
                let (name, public_id, system_id) =
                    (name.clone(), public_id.clone(), system_id.clone());
                self.create_doctype_in(owner, name, public_id, system_id)
            }
            NodeData::DocumentFragment { .. } => self.create_document_fragment_in(owner),
            NodeData::Document(_) => unreachable!("guarded by clone_subtree_into"),
        };
        if deep {
            stack.push((node, copy));
        }
        copy
    }

    // === Node comparison algorithms ===

    /// Spec `isEqualNode`: deep structural equality.
    #[must_use]
    pub fn is_equal_node(&self, a: NodeId, b: NodeId) -> bool {
        if a == b {
            return true;
        }
        let (da, db) = (self.node(a).data(), self.node(b).data());
        if da.kind() != db.kind() {
            return false;
        }
        let shallow_equal = match (da, db) {
            (
                NodeData::Doctype {
                    name: na,
                    public_id: pa,
                    system_id: sa,
                },
                NodeData::Doctype {
                    name: nb,
                    public_id: pb,
                    system_id: sb,
                },
            ) => na == nb && pa == pb && sa == sb,
            (NodeData::Element(ea), NodeData::Element(eb)) => {
                ea.name == eb.name
                    && ea.attrs.len() == eb.attrs.len()
                    && ea.attrs.iter().all(|attr| {
                        eb.attrs.iter().any(|other| {
                            attr.name.ns == other.name.ns
                                && attr.name.local == other.name.local
                                && attr.value == other.value
                        })
                    })
            }
            (NodeData::Text(ta), NodeData::Text(tb))
            | (NodeData::CdataSection(ta), NodeData::CdataSection(tb))
            | (NodeData::Comment(ta), NodeData::Comment(tb)) => ta == tb,
            (
                NodeData::ProcessingInstruction {
                    target: ta,
                    data: da,
                },
                NodeData::ProcessingInstruction {
                    target: tb,
                    data: db,
                },
            ) => ta == tb && da == db,
            (NodeData::Document(_), NodeData::Document(_))
            | (NodeData::DocumentFragment { .. }, NodeData::DocumentFragment { .. }) => true,
            _ => false,
        };
        if !shallow_equal {
            return false;
        }
        let mut ca = self.node(a).first_child;
        let mut cb = self.node(b).first_child;
        loop {
            match (ca, cb) {
                (None, None) => return true,
                (Some(x), Some(y)) => {
                    if !self.is_equal_node(x, y) {
                        return false;
                    }
                    ca = self.node(x).next_sibling;
                    cb = self.node(y).next_sibling;
                }
                _ => return false,
            }
        }
    }

    /// Spec `compareDocumentPosition` (no Attr nodes in this engine).
    #[must_use]
    pub fn compare_document_position(&self, node: NodeId, other: NodeId) -> u16 {
        if node == other {
            return 0;
        }
        let root_of = |id: NodeId| self.inclusive_ancestors(id).last().unwrap_or(id);
        if root_of(node) != root_of(other) {
            // Disconnected: consistent, implementation-specific order.
            let key = |id: NodeId| (id.index(), id.generation());
            let order = if key(other) < key(node) {
                DOCUMENT_POSITION_PRECEDING
            } else {
                DOCUMENT_POSITION_FOLLOWING
            };
            return DOCUMENT_POSITION_DISCONNECTED
                | DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC
                | order;
        }
        if self.ancestors(node).any(|a| a == other) {
            return DOCUMENT_POSITION_CONTAINS | DOCUMENT_POSITION_PRECEDING;
        }
        if self.ancestors(other).any(|a| a == node) {
            return DOCUMENT_POSITION_CONTAINED_BY | DOCUMENT_POSITION_FOLLOWING;
        }
        let root = root_of(node);
        for descendant in self.inclusive_descendants(root) {
            if descendant == other {
                return DOCUMENT_POSITION_PRECEDING;
            }
            if descendant == node {
                return DOCUMENT_POSITION_FOLLOWING;
            }
        }
        unreachable!("both nodes share a root, one must come first")
    }

    /// Spec `normalize()`: merges contiguous exclusive Text nodes in the
    /// subtree of `node` and removes empty ones. Returns the detached text
    /// nodes so the caller can decide their fate (freeing, in the bindings).
    ///
    /// The `NodeKind::Text` tests below are exact, not an oversight: the spec
    /// says *exclusive* Text node, which is a Text that is **not** a
    /// CDATASection. A CDATASection neither merges nor is dropped when empty.
    pub fn normalize(&mut self, node: NodeId) -> Vec<NodeId> {
        let descendants: Vec<NodeId> = self.inclusive_descendants(node).collect();
        let mut detached = Vec::new();
        for id in descendants {
            // Skip nodes a previous merge already removed.
            if detached.contains(&id) {
                continue;
            }
            if self.node(id).data().kind() != NodeKind::Text {
                continue;
            }
            let mut data = match self.node(id).character_data() {
                Some(d) => d.clone(),
                None => continue,
            };
            if data.is_empty() {
                self.remove(id);
                detached.push(id);
                continue;
            }
            // Merge the contiguous run of following text siblings.
            let mut run = Vec::new();
            let mut next = self.node(id).next_sibling;
            while let Some(sibling) = next {
                if self.node(sibling).data().kind() != NodeKind::Text {
                    break;
                }
                run.push(sibling);
                next = self.node(sibling).next_sibling;
            }
            if run.is_empty() {
                continue;
            }
            for sibling in &run {
                if let Some(d) = self.node(*sibling).character_data() {
                    let d = d.clone();
                    data.push_tendril(&d);
                }
            }
            self.set_character_data(id, data);
            for sibling in run {
                self.remove(sibling);
                detached.push(sibling);
            }
        }
        detached
    }

    /// Ensures a `<template>` element has its contents fragment (parser
    /// creates it eagerly; `document.createElement` goes through here).
    pub fn ensure_template_contents(&mut self, host: NodeId) -> NodeId {
        if let Some(el) = self.node(host).as_element()
            && let Some(contents) = el.template_contents
        {
            return contents;
        }
        self.create_template_contents(host)
    }

    /// Copies the children of `source_root` (living in `source`) into this
    /// tree as children of `target`, recursively, including template
    /// contents. Used by the fragment-parsing entry point (`innerHTML =`).
    pub(crate) fn graft_subtree_children(
        &mut self,
        source: &DomTree,
        source_root: NodeId,
        target: NodeId,
    ) {
        let owner = self.node_document(target);
        for child in source.children(source_root) {
            let copy = self.copy_node_from(source, child, owner);
            self.insert_internal(copy, target, None, false);
            self.graft_subtree_children(source, child, copy);
        }
    }

    fn copy_node_from(&mut self, source: &DomTree, id: NodeId, owner: NodeId) -> NodeId {
        match source.node(id).data() {
            NodeData::Element(el) => {
                // No upgrade on creation: the fragment was parsed in a
                // browsing-context-less document, and grafting it back is still
                // *creation*, not insertion. `insert_internal` below upgrades it
                // through `note_custom_element_connect` iff the target tree is
                // connected — the spec's rule, and the reason a detached
                // `innerHTML =` leaves the element `Undefined` (ADR-0021 §6).
                let copy =
                    self.create_element_in_inner(owner, el.name.clone(), el.attrs.clone(), false);
                // Fragment-parsed scripts (innerHTML/outerHTML/adjacent HTML)
                // are inert. Preserve the parser's already-started state when
                // grafting them into the live document; ordinary cloneNode has
                // separate semantics and still goes through clone_node_inner.
                if el.script_already_started {
                    self.mark_script_already_started(copy);
                }
                if let Some(contents) = el.template_contents {
                    let contents_copy = self.create_template_contents(copy);
                    self.graft_subtree_children(source, contents, contents_copy);
                }
                copy
            }
            NodeData::Text(t) => self.create_text_in(owner, t.clone()),
            NodeData::CdataSection(t) => self.create_cdata_section_in(owner, t.clone()),
            NodeData::Comment(t) => self.create_comment_in(owner, t.clone()),
            NodeData::ProcessingInstruction { target, data } => {
                self.create_processing_instruction_in(owner, target.clone(), data.clone())
            }
            NodeData::Doctype {
                name,
                public_id,
                system_id,
            } => self.create_doctype_in(owner, name.clone(), public_id.clone(), system_id.clone()),
            NodeData::DocumentFragment { .. } => self.create_document_fragment_in(owner),
            NodeData::Document(_) => unreachable!("documents are never grafted"),
        }
    }

    // === Validation (DOM spec §4.2.3) ===

    fn ensure_common_insert_validity(
        &self,
        node: NodeId,
        parent: NodeId,
        child: Option<NodeId>,
    ) -> Result<(), DomException> {
        match self.node(parent).data().kind() {
            NodeKind::Document | NodeKind::DocumentFragment | NodeKind::Element => {}
            _ => {
                return Err(hierarchy_error(
                    "parent must be a Document, DocumentFragment, or Element",
                ));
            }
        }
        if self.is_host_including_inclusive_ancestor(node, parent) {
            return Err(hierarchy_error(
                "node is a host-including inclusive ancestor of parent",
            ));
        }
        if let Some(c) = child
            && self.node(c).parent != Some(parent)
        {
            return Err(DomException::new(
                DomExceptionKind::NotFoundError,
                "reference child is not a child of parent",
            ));
        }
        match self.node(node).data().kind() {
            NodeKind::DocumentFragment
            | NodeKind::Doctype
            | NodeKind::Element
            | NodeKind::Text
            | NodeKind::CdataSection
            | NodeKind::Comment
            | NodeKind::ProcessingInstruction => {}
            NodeKind::Document => {
                return Err(hierarchy_error("a Document cannot be inserted"));
            }
        }
        let parent_is_document = self.node(parent).data().kind() == NodeKind::Document;
        if parent_is_document && is_text_kind(self.node(node).data().kind()) {
            return Err(hierarchy_error(
                "a Text node cannot be a child of a Document",
            ));
        }
        if !parent_is_document && self.node(node).data().kind() == NodeKind::Doctype {
            return Err(hierarchy_error(
                "a doctype can only be a child of a Document",
            ));
        }
        Ok(())
    }

    fn ensure_pre_insert_validity(
        &self,
        node: NodeId,
        parent: NodeId,
        child: Option<NodeId>,
    ) -> Result<(), DomException> {
        self.ensure_common_insert_validity(node, parent, child)?;
        if self.node(parent).data().kind() != NodeKind::Document {
            return Ok(());
        }

        let has_element_child = |p: NodeId| {
            self.children(p)
                .any(|c| self.node(c).data().kind() == NodeKind::Element)
        };
        let doctype_follows = |c: Option<NodeId>| {
            let mut current = c;
            while let Some(id) = current {
                if self.node(id).data().kind() == NodeKind::Doctype {
                    return true;
                }
                current = self.node(id).next_sibling;
            }
            false
        };

        match self.node(node).data().kind() {
            NodeKind::DocumentFragment => {
                let element_children = self
                    .children(node)
                    .filter(|&c| self.node(c).data().kind() == NodeKind::Element)
                    .count();
                let has_text_child = self.children(node).any(|c| self.node(c).is_text());
                if element_children > 1 || has_text_child {
                    return Err(hierarchy_error(
                        "fragment with multiple elements or text cannot be inserted into a Document",
                    ));
                }
                if element_children == 1
                    && (has_element_child(parent)
                        || child.is_some_and(|c| self.node(c).data().kind() == NodeKind::Doctype)
                        || doctype_follows(child))
                {
                    return Err(hierarchy_error(
                        "Document already has a document element or a doctype would follow",
                    ));
                }
            }
            NodeKind::Element
                if (has_element_child(parent)
                    || child.is_some_and(|c| self.node(c).data().kind() == NodeKind::Doctype)
                    || doctype_follows(child)) =>
            {
                return Err(hierarchy_error(
                    "Document already has a document element or a doctype would follow",
                ));
            }
            NodeKind::Doctype => {
                let has_doctype = self
                    .children(parent)
                    .any(|c| self.node(c).data().kind() == NodeKind::Doctype);
                let element_precedes = match child {
                    Some(c) => {
                        let mut found = false;
                        for sibling in self.children(parent) {
                            if sibling == c {
                                break;
                            }
                            if self.node(sibling).data().kind() == NodeKind::Element {
                                found = true;
                                break;
                            }
                        }
                        found
                    }
                    None => has_element_child(parent),
                };
                if has_doctype || element_precedes {
                    return Err(hierarchy_error(
                        "Document already has a doctype or an element precedes the insertion point",
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn ensure_replace_validity(
        &self,
        node: NodeId,
        parent: NodeId,
        child: NodeId,
    ) -> Result<(), DomException> {
        // Steps 1–2 and 4–5 are shared with pre-insert; step 3 requires the
        // replaced child to be a child of parent.
        match self.node(parent).data().kind() {
            NodeKind::Document | NodeKind::DocumentFragment | NodeKind::Element => {}
            _ => {
                return Err(hierarchy_error(
                    "parent must be a Document, DocumentFragment, or Element",
                ));
            }
        }
        if self.is_host_including_inclusive_ancestor(node, parent) {
            return Err(hierarchy_error(
                "node is a host-including inclusive ancestor of parent",
            ));
        }
        if self.node(child).parent != Some(parent) {
            return Err(DomException::new(
                DomExceptionKind::NotFoundError,
                "child to be replaced is not a child of parent",
            ));
        }
        match self.node(node).data().kind() {
            NodeKind::DocumentFragment
            | NodeKind::Doctype
            | NodeKind::Element
            | NodeKind::Text
            | NodeKind::CdataSection
            | NodeKind::Comment
            | NodeKind::ProcessingInstruction => {}
            NodeKind::Document => {
                return Err(hierarchy_error("a Document cannot be inserted"));
            }
        }
        let parent_is_document = self.node(parent).data().kind() == NodeKind::Document;
        if parent_is_document && is_text_kind(self.node(node).data().kind()) {
            return Err(hierarchy_error(
                "a Text node cannot be a child of a Document",
            ));
        }
        if !parent_is_document && self.node(node).data().kind() == NodeKind::Doctype {
            return Err(hierarchy_error(
                "a doctype can only be a child of a Document",
            ));
        }
        if !parent_is_document {
            return Ok(());
        }

        let has_other_element_child = |exclude: NodeId| {
            self.children(parent)
                .any(|c| c != exclude && self.node(c).data().kind() == NodeKind::Element)
        };
        let doctype_follows_child = || {
            let mut current = self.node(child).next_sibling;
            while let Some(id) = current {
                if self.node(id).data().kind() == NodeKind::Doctype {
                    return true;
                }
                current = self.node(id).next_sibling;
            }
            false
        };

        match self.node(node).data().kind() {
            NodeKind::DocumentFragment => {
                let element_children = self
                    .children(node)
                    .filter(|&c| self.node(c).data().kind() == NodeKind::Element)
                    .count();
                let has_text_child = self.children(node).any(|c| self.node(c).is_text());
                if element_children > 1 || has_text_child {
                    return Err(hierarchy_error(
                        "fragment with multiple elements or text cannot be inserted into a Document",
                    ));
                }
                if element_children == 1
                    && (has_other_element_child(child) || doctype_follows_child())
                {
                    return Err(hierarchy_error(
                        "Document already has a document element or a doctype would follow",
                    ));
                }
            }
            NodeKind::Element if (has_other_element_child(child) || doctype_follows_child()) => {
                return Err(hierarchy_error(
                    "Document already has a document element or a doctype would follow",
                ));
            }
            NodeKind::Doctype => {
                let has_other_doctype = self
                    .children(parent)
                    .any(|c| c != child && self.node(c).data().kind() == NodeKind::Doctype);
                let element_precedes = {
                    let mut found = false;
                    for sibling in self.children(parent) {
                        if sibling == child {
                            break;
                        }
                        if self.node(sibling).data().kind() == NodeKind::Element {
                            found = true;
                            break;
                        }
                    }
                    found
                };
                if has_other_doctype || element_precedes {
                    return Err(hierarchy_error(
                        "Document already has a doctype or an element precedes the insertion point",
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }

    // === Internal mutation primitives (the single mutation code path) ===

    /// Spec `insert`: mutates the tree, queues observer records (unless
    /// suppressed), updates connectedness, and runs the invalidation hook.
    ///
    /// Callers must have validated the operation (or, for the parser sink,
    /// be trusted to uphold the tree builder's invariants).
    pub(crate) fn insert_internal(
        &mut self,
        node: NodeId,
        parent: NodeId,
        child: Option<NodeId>,
        suppress_observers: bool,
    ) {
        let is_fragment =
            self.node(node).data().kind() == NodeKind::DocumentFragment && node != self.document;
        let nodes: Vec<NodeId> = if is_fragment {
            self.children(node).collect()
        } else {
            vec![node]
        };
        if nodes.is_empty() {
            return;
        }
        if is_fragment {
            for &n in &nodes {
                self.remove_internal(n, true);
            }
            self.queue_child_list_record(
                node,
                RecordContents {
                    added_nodes: Vec::new(),
                    removed_nodes: nodes.clone(),
                    previous_sibling: None,
                    next_sibling: None,
                },
            );
        }

        let previous_sibling = match child {
            Some(c) => self.node(c).prev_sibling,
            None => self.node(parent).last_child,
        };

        let owner = self.node_document(parent);
        for &n in &nodes {
            // Spec "adopt": a node still attached elsewhere is removed from
            // its old parent first, observably (moving a node produces both
            // a removal and an addition record).
            if !is_fragment && self.node(n).parent.is_some() {
                self.remove_internal(n, false);
            }
            // …and is adopted into the parent's node document. Doing it here,
            // on the single insertion path, gives cross-document `appendChild`,
            // the parser sink and cloning the same adoption for free.
            self.set_owner_composed(n, owner);
            self.attach_to_sibling_list(n, parent, child);
            self.propagate_connectedness(n);
            self.note_subtree_mutation(n);
            self.note_shadow_mutation(n);
            // The subtree's element state depends on where it now sits: a
            // control inserted under `<fieldset disabled>` becomes `:disabled`,
            // and an `<option>` inserted into an empty `<select>` becomes the
            // selected one.
            self.update_element_state_subtree(n);
            self.note_option_list_changed(n);
            if self.node(n).is_connected() {
                self.note_stylesheet_owners(n, false);
                self.note_image_owners(n);
                self.note_script_owners(n);
                self.note_custom_element_connect(n);
            }
        }
        self.note_children_changed(parent);

        if !suppress_observers {
            self.queue_child_list_record(
                parent,
                RecordContents {
                    added_nodes: nodes,
                    removed_nodes: Vec::new(),
                    previous_sibling,
                    next_sibling: child,
                },
            );
        }
    }

    /// Queues a [`StyleUpdate`] for each `<style>`/`<link rel=stylesheet>` in
    /// the subtree rooted at `root`. `removed` selects the *Removed* variants.
    fn note_stylesheet_owners(&mut self, root: NodeId, removed: bool) {
        let ids: Vec<NodeId> = self.inclusive_descendants(root).collect();
        for id in ids {
            let (is_style, is_sheet_link) = {
                let node = self.node(id);
                (node.is_style_element(), node.is_stylesheet_link())
            };
            if is_style {
                self.push_style_update(if removed {
                    StyleUpdate::StyleElementRemoved(id)
                } else {
                    StyleUpdate::StyleElement(id)
                });
            } else if is_sheet_link {
                self.push_style_update(if removed {
                    StyleUpdate::LinkElementRemoved(id)
                } else {
                    StyleUpdate::LinkElement(id)
                });
            }
        }
    }

    /// Spec `remove`: mutates the tree, queues observer records (unless
    /// suppressed), registers transient observers, updates connectedness,
    /// and runs the invalidation hook.
    pub(crate) fn remove_internal(&mut self, node: NodeId, suppress_observers: bool) {
        let Some(parent) = self.node(node).parent else {
            return;
        };
        let old_prev = self.node(node).prev_sibling;
        let old_next = self.node(node).next_sibling;

        // Spec: for each inclusive ancestor of parent with a subtree
        // observer, append a transient registered observer to node, so the
        // detached subtree stays observed until the next takeRecords.
        let ancestors: Vec<NodeId> = self.inclusive_ancestors(parent).collect();
        self.observers.register_transients(&ancestors, node);

        // Queue "removed" updates for any connected stylesheets leaving the
        // tree, and `disconnectedCallback` reactions for connected custom
        // elements — both must be captured while `is_connected` still holds.
        if self.node(node).is_connected() {
            self.note_stylesheet_owners(node, true);
            self.note_custom_element_disconnect(node);
        }
        // A slot leaving a shadow tree re-assigns the host's slottables;
        // hint them while the containing-root chain still holds.
        self.note_shadow_mutation(node);

        self.detach_from_sibling_list(node);
        self.propagate_connectedness(node);
        self.note_children_changed(parent);
        // Leaving a `<fieldset disabled>` re-enables the subtree, and removing
        // the selected `<option>` leaves its `<select>` needing a reset — which
        // is why this runs against the *old* parent, after the detach.
        self.update_element_state_subtree(node);
        self.note_option_list_changed(parent);
        self.clear_focus_if_disconnected(parent);
        self.clear_pointer_state_if_disconnected();

        if !suppress_observers {
            self.queue_child_list_record(
                parent,
                RecordContents {
                    added_nodes: Vec::new(),
                    removed_nodes: vec![node],
                    previous_sibling: old_prev,
                    next_sibling: old_next,
                },
            );
        }
    }

    fn attach_to_sibling_list(&mut self, node: NodeId, parent: NodeId, before: Option<NodeId>) {
        debug_assert!(self.node(node).parent.is_none(), "node must be detached");
        match before {
            Some(next) => {
                let prev = self.node(next).prev_sibling;
                {
                    let n = self.arena.node_mut(node);
                    n.parent = Some(parent);
                    n.prev_sibling = prev;
                    n.next_sibling = Some(next);
                }
                self.arena.node_mut(next).prev_sibling = Some(node);
                match prev {
                    Some(p) => self.arena.node_mut(p).next_sibling = Some(node),
                    None => self.arena.node_mut(parent).first_child = Some(node),
                }
            }
            None => {
                let last = self.node(parent).last_child;
                {
                    let n = self.arena.node_mut(node);
                    n.parent = Some(parent);
                    n.prev_sibling = last;
                    n.next_sibling = None;
                }
                match last {
                    Some(l) => self.arena.node_mut(l).next_sibling = Some(node),
                    None => self.arena.node_mut(parent).first_child = Some(node),
                }
                self.arena.node_mut(parent).last_child = Some(node);
            }
        }
    }

    fn detach_from_sibling_list(&mut self, node: NodeId) {
        let (parent, prev, next) = {
            let n = self.node(node);
            (n.parent, n.prev_sibling, n.next_sibling)
        };
        let Some(parent) = parent else { return };
        match prev {
            Some(p) => self.arena.node_mut(p).next_sibling = next,
            None => self.arena.node_mut(parent).first_child = next,
        }
        match next {
            Some(nx) => self.arena.node_mut(nx).prev_sibling = prev,
            None => self.arena.node_mut(parent).last_child = prev,
        }
        let n = self.arena.node_mut(node);
        n.parent = None;
        n.prev_sibling = None;
        n.next_sibling = None;
    }

    /// Recomputes `IS_CONNECTED` for the subtree rooted at `node` from its
    /// (new) parent's connectedness. The walk is *composed*: shadow trees
    /// hanging off elements in the subtree follow their host's connectedness
    /// (spec: a shadow root's connectedness derives from its host).
    fn propagate_connectedness(&mut self, node: NodeId) {
        let connected = match self.node(node).parent {
            Some(p) => self.node(p).is_connected(),
            None => node == self.document,
        };
        self.set_connectedness_composed(node, connected);
    }

    /// Sets `IS_CONNECTED` on `root` and all its composed descendants
    /// (recursing through shadow roots), maintaining the `id` index for
    /// light-tree elements only — shadow ids are scoped to their tree and
    /// must not leak into the document's `getElementById`.
    fn set_connectedness_composed(&mut self, root: NodeId, connected: bool) {
        let ids: Vec<NodeId> = self.inclusive_descendants(root).collect();
        let track_pins = !self.pins.is_empty();
        for id in ids {
            let was_connected = self.node(id).is_connected();
            self.arena
                .node_mut(id)
                .flags
                .set(NodeFlags::IS_CONNECTED, connected);
            // A pinned (JS-wrapped) node changing connectedness: hand the
            // bindings layer a note so it can strongly retain the wrapper while
            // the node is connected — preserving author-set expando properties
            // across GC, which jQuery/Angular rely on — and release it on
            // disconnect so detached subtrees still free (design §5.3).
            if track_pins && was_connected != connected && self.pins.contains_key(&id) {
                self.pinned_connectivity.push((id, connected));
            }
            // Entering or leaving the document is exactly when an element
            // enters or leaves the `id` index.
            let el_id = self.node(id).as_element().and_then(|el| el.id()).cloned();
            if let Some(el_id) = el_id
                && self.containing_shadow_root(id).is_none()
            {
                if connected {
                    self.index_add(&el_id, id);
                } else {
                    self.index_remove(&el_id, id);
                }
            }
            let shadow = self
                .node(id)
                .as_element()
                .and_then(ElementData::shadow_root);
            if let Some(shadow) = shadow {
                self.set_connectedness_composed(shadow, connected);
            }
        }
    }

    // === Invalidation hook ===
    //
    // Every mutation funnels through these two methods. In Phase 4 they
    // additionally translate mutations into stylo restyle hints.

    /// A structural change happened directly under `parent`.
    fn note_children_changed(&mut self, parent: NodeId) {
        self.style_version.set(self.style_version.get() + 1);
        self.bump_structure_version();
        self.arena
            .node_mut(parent)
            .flags
            .insert(NodeFlags::STYLE_DIRTY | NodeFlags::LAYOUT_DIRTY | NodeFlags::PAINT_DIRTY);
        self.mark_dirty_ancestors(parent);
        self.note_stylo_restyle(parent);
        if self.node(parent).is_connected()
            && self
                .node(parent)
                .as_element()
                .is_some_and(|el| el.is_html_element() && el.name.local == local_name!("script"))
        {
            self.script_updates.push(parent);
        }
    }

    /// `node` itself changed (was inserted, or its data/attributes changed).
    pub(crate) fn note_subtree_mutation(&mut self, node: NodeId) {
        self.style_version.set(self.style_version.get() + 1);
        self.arena
            .node_mut(node)
            .flags
            .insert(NodeFlags::STYLE_DIRTY | NodeFlags::LAYOUT_DIRTY | NodeFlags::PAINT_DIRTY);
        self.mark_dirty_ancestors(node);
        self.note_stylo_restyle(node);
    }

    fn mark_dirty_ancestors(&mut self, node: NodeId) {
        // Composed ancestors: crossing shadow-fragment → host keeps the
        // dirty/`dirty_descendants` chain alive up to the document root for
        // mutations inside shadow trees.
        let ancestors: Vec<NodeId> = {
            let mut out = Vec::new();
            let mut current = self.composed_parent(node);
            while let Some(id) = current {
                out.push(id);
                current = self.composed_parent(id);
            }
            out
        };
        // The engine gate (`NodeFlags::HAS_DIRTY_DESCENDANT`) is only set here
        // and cleared by layout as it walks the box tree, so an ancestor
        // carrying it guarantees all of *its* ancestors carry it too. We can
        // stop at the first one already set.
        for &a in &ancestors {
            let flags = &mut self.arena.node_mut(a).flags;
            if flags.contains(NodeFlags::HAS_DIRTY_DESCENDANT) {
                break;
            }
            flags.insert(NodeFlags::HAS_DIRTY_DESCENDANT);
        }
        // Stylo's `dirty_descendants` bit does NOT satisfy that invariant, so we
        // must NOT early-break on it (M3). When an element is restyled to
        // `display:none`, stylo's `clear_descendant_data` drops the subtree's
        // cascade data but `clear_descendant_bits` unsets only the subtree
        // *root's* own bit — descendants keep a stale `dirty_descendants` bit
        // while their parent's is cleared, breaking "bit set ⇒ ancestors set"
        // (stylo `traversal.rs`). Early-breaking at such a stale bit would leave
        // a higher ancestor unmarked and let a restyle be pruned. Propagating
        // unconditionally to the root is O(depth) — the ancestors are already
        // collected — and keeps the chain intact regardless of stale bits.
        for &a in &ancestors {
            if let Some(el) = self.arena.node_mut(a).as_element_mut() {
                el.stylo.dirty_descendants.set(true);
            }
        }
    }

    // === Attributes ===

    /// The element's `id` as currently indexed, when an `id`-attribute write on
    /// a *connected* element is about to change it. `None` in every other case
    /// (other attributes, detached elements, elements without an id).
    fn indexed_id(&self, element: NodeId, attr_local: &html5ever::LocalName) -> Option<Atom> {
        if *attr_local != local_name!("id")
            || !self.node(element).is_connected()
            || self.containing_shadow_root(element).is_some()
        {
            return None;
        }
        self.node(element)
            .as_element()
            .and_then(|el| el.id())
            .cloned()
    }

    /// Moves `element` from `old_id` to its post-mutation id in the index.
    /// Connectedness cannot change across an attribute write, so the guard here
    /// agrees with the one [`DomTree::indexed_id`] applied before it.
    fn reindex_id(
        &mut self,
        element: NodeId,
        attr_local: &html5ever::LocalName,
        old_id: Option<Atom>,
    ) {
        if *attr_local != local_name!("id")
            || !self.node(element).is_connected()
            || self.containing_shadow_root(element).is_some()
        {
            return;
        }
        let new_id = self
            .node(element)
            .as_element()
            .and_then(|el| el.id())
            .cloned();
        if old_id == new_id {
            return;
        }
        if let Some(old) = old_id {
            self.index_remove(&old, element);
        }
        if let Some(new) = new_id {
            self.index_add(&new, element);
        }
    }

    /// Sets an attribute, updating selector caches, queueing an `attributes`
    /// mutation record, and running the invalidation hook.
    pub fn set_attribute(&mut self, element: NodeId, name: QualName, value: StrTendril) {
        let old_value = {
            let Some(el) = self.node(element).as_element() else {
                return;
            };
            el.attr(&name).cloned()
        };
        // Capture the pieces of an `attributeChangedCallback` reaction before
        // `name`/`value`/`old_value` are moved below.
        let ce_old = old_value.as_ref().map(std::string::ToString::to_string);
        let ce_new = Some(value.to_string());
        let ce_namespace = attr_namespace(&name);
        self.queue_attribute_record(element, &name, old_value);
        // Snapshot the pre-mutation state for stylo's invalidation.
        self.snapshot_element(element);
        let attr_local = name.local.clone();
        let old_id = self.indexed_id(element, &attr_local);
        let lock = &self.style_lock;
        let url = &self.url_extra_data;
        if let Some(el) = self.arena.node_mut(element).as_element_mut() {
            match el.attrs.iter_mut().find(|a| a.name == name) {
                Some(attr) => attr.value = value,
                None => el.attrs.push(Attribute { name, value }),
            }
            el.refresh_selector_caches(lock, url);
        }
        self.reindex_id(element, &attr_local, old_id);
        if !is_style_only_attr(&attr_local) {
            self.bump_structure_version();
        }
        self.note_subtree_mutation(element);
        self.note_style_owner_attr(element, &attr_local);
        self.note_script_owner_attr(element, &attr_local);
        self.note_slot_attr(element, &attr_local);
        self.note_form_attr(element, &attr_local);
        self.push_attribute_changed_if_custom(
            element,
            attr_local.to_string(),
            ce_namespace,
            ce_old,
            ce_new,
        );
    }

    /// Re-hints slot assignment when a `slot` attribute changes on a shadow
    /// host's light child, or a `name` attribute changes on a `<slot>` in a
    /// shadow tree.
    fn note_slot_attr(&mut self, element: NodeId, attr_local: &html5ever::LocalName) {
        if *attr_local == local_name!("slot") {
            let host = self.node(element).parent().filter(|&p| {
                self.node(p)
                    .as_element()
                    .is_some_and(|el| el.shadow_root().is_some())
            });
            if let Some(host) = host {
                self.note_slot_assignment_changed(host);
            }
        } else if *attr_local == local_name!("name") && self.is_slot_element(element) {
            let host = self
                .containing_shadow_root(element)
                .and_then(|root| self.shadow_host(root));
            if let Some(host) = host {
                self.note_slot_assignment_changed(host);
            }
        }
    }

    /// Removes an attribute; returns its old value if it was present.
    pub fn remove_attribute(&mut self, element: NodeId, name: &QualName) -> Option<StrTendril> {
        let old_value = {
            let el = self.node(element).as_element()?;
            el.attr(name).cloned()?
        };
        let ce_old = Some(old_value.to_string());
        let ce_namespace = attr_namespace(name);
        self.queue_attribute_record(element, name, Some(old_value.clone()));
        self.snapshot_element(element);
        let old_id = self.indexed_id(element, &name.local);
        let lock = &self.style_lock;
        let url = &self.url_extra_data;
        if let Some(el) = self.arena.node_mut(element).as_element_mut() {
            el.attrs.retain(|a| a.name != *name);
            el.refresh_selector_caches(lock, url);
        }
        self.reindex_id(element, &name.local, old_id);
        if !is_style_only_attr(&name.local) {
            self.bump_structure_version();
        }
        self.note_subtree_mutation(element);
        self.note_style_owner_attr(element, &name.local);
        self.note_script_owner_attr(element, &name.local);
        self.note_slot_attr(element, &name.local);
        self.note_form_attr(element, &name.local);
        self.push_attribute_changed_if_custom(
            element,
            name.local.to_string(),
            ce_namespace,
            ce_old,
            None,
        );
        Some(old_value)
    }

    /// Enqueues an `attributeChangedCallback` reaction when `element` is a
    /// `Custom` element. The `observedAttributes` filter is applied later, on
    /// the bindings side, when the reaction queue drains.
    fn push_attribute_changed_if_custom(
        &mut self,
        element: NodeId,
        name: String,
        namespace: Option<String>,
        old: Option<String>,
        new: Option<String>,
    ) {
        if self
            .node(element)
            .as_element()
            .is_some_and(|el| el.custom_state == CustomElementState::Custom)
        {
            self.custom_reactions
                .push(CustomElementReaction::AttributeChanged {
                    node: element,
                    name,
                    namespace,
                    old,
                    new,
                });
        }
    }

    /// Queues a [`StyleUpdate`] when an attribute change affects whether a
    /// connected `<style>`/`<link rel=stylesheet>` contributes to the style
    /// set, and an image update when a connected `<img>`'s `src` changes.
    fn note_style_owner_attr(&mut self, element: NodeId, attr_local: &html5ever::LocalName) {
        // `loading` alongside `src`: writing `img.loading = "eager"` must undefer
        // an image the lazy loader is holding back (oxidepage-page, ADR-0014).
        if (*attr_local == local_name!("src") || &**attr_local == "loading")
            && self.node(element).is_connected()
            && self.node(element).is_image_element()
        {
            self.push_image_update(element);
        }
        let relevant = *attr_local == local_name!("rel")
            || *attr_local == local_name!("href")
            || *attr_local == local_name!("media")
            || *attr_local == local_name!("disabled")
            || *attr_local == local_name!("type");
        if !relevant {
            return;
        }
        let (is_style, is_link, is_sheet) = {
            let node = self.node(element);
            if !node.is_connected() {
                return;
            }
            let is_link = node
                .as_element()
                .is_some_and(|el| el.is_html_element() && el.name.local == local_name!("link"));
            (node.is_style_element(), is_link, node.is_stylesheet_link())
        };
        if is_style {
            self.push_style_update(StyleUpdate::StyleElement(element));
        } else if is_link {
            self.push_style_update(if is_sheet {
                StyleUpdate::LinkElement(element)
            } else {
                StyleUpdate::LinkElementRemoved(element)
            });
        }
    }

    /// Queues a connected, not-yet-started script when preparation-relevant
    /// attributes change. Page re-reads the final state when it drains.
    fn note_script_owner_attr(&mut self, element: NodeId, attr_local: &html5ever::LocalName) {
        let relevant = matches!(
            &**attr_local,
            "src" | "type" | "async" | "defer" | "nomodule" | "crossorigin"
        );
        if relevant
            && self.node(element).is_connected()
            && !self.script_already_started(element)
            && self
                .node(element)
                .as_element()
                .is_some_and(|el| el.is_html_element() && el.name.local == local_name!("script"))
        {
            self.script_updates.push(element);
        }
    }

    /// Adds each attribute only if absent (html5ever `TreeSink` semantics).
    pub(crate) fn add_attrs_if_missing(&mut self, element: NodeId, attrs: Vec<Attribute>) {
        for attr in attrs {
            let missing = self
                .node(element)
                .as_element()
                .is_some_and(|el| el.attr(&attr.name).is_none());
            if missing {
                self.set_attribute(element, attr.name, attr.value);
            }
        }
    }

    // === Character data ===

    /// Replaces the data of a Text/Comment/PI node, queueing a
    /// `characterData` record and running the invalidation hook.
    pub fn set_character_data(&mut self, node: NodeId, new_data: StrTendril) {
        let old = match self.node(node).character_data() {
            Some(d) => d.clone(),
            None => return,
        };
        self.queue_character_data_record(node, old);
        match &mut self.arena.node_mut(node).data {
            NodeData::Text(t) | NodeData::CdataSection(t) | NodeData::Comment(t) => *t = new_data,
            NodeData::ProcessingInstruction { data, .. } => *data = new_data,
            _ => unreachable!("character_data() returned Some for this node"),
        }
        self.bump_structure_version();
        self.note_subtree_mutation(node);
        // Text under a connected `<style>` changes its stylesheet contents.
        if let Some(parent) = self.node(node).parent()
            && self.node(parent).is_style_element()
            && self.node(parent).is_connected()
        {
            self.push_style_update(StyleUpdate::StyleElement(parent));
        }
        if let Some(parent) = self.node(node).parent()
            && self.node(parent).is_connected()
            && !self.script_already_started(parent)
            && self
                .node(parent)
                .as_element()
                .is_some_and(|el| el.is_html_element() && el.name.local == local_name!("script"))
        {
            self.script_updates.push(parent);
        }
    }

    /// Appends to a text node's data (parser text merging), via the same
    /// observable path as any other character-data mutation.
    pub(crate) fn append_to_text(&mut self, node: NodeId, extra: &StrTendril) {
        let mut data = match self.node(node).character_data() {
            Some(d) => d.clone(),
            None => return,
        };
        data.push_tendril(extra);
        self.set_character_data(node, data);
    }

    // === Observer record queuing (delegates to the registry) ===

    fn queue_child_list_record(&mut self, target: NodeId, contents: RecordContents) {
        let ancestors: Vec<NodeId> = self.inclusive_ancestors(target).collect();
        self.observers
            .queue_record(&ancestors, target, RecordKind::ChildList, contents);
    }

    fn queue_attribute_record(
        &mut self,
        target: NodeId,
        name: &QualName,
        old_value: Option<StrTendril>,
    ) {
        let ancestors: Vec<NodeId> = self.inclusive_ancestors(target).collect();
        self.observers.queue_record(
            &ancestors,
            target,
            RecordKind::Attributes {
                name: name.clone(),
                old_value,
            },
            RecordContents::default(),
        );
    }

    fn queue_character_data_record(&mut self, target: NodeId, old_value: StrTendril) {
        let ancestors: Vec<NodeId> = self.inclusive_ancestors(target).collect();
        self.observers.queue_record(
            &ancestors,
            target,
            RecordKind::CharacterData { old_value },
            RecordContents::default(),
        );
    }
}
