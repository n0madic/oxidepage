//! The page's browsing contexts (ADR-0035).
//!
//! A page is a *tree* of browsing contexts: the top-level one it navigated,
//! plus one per `<iframe>` that has loaded. They share a single [`DomTree`]
//! arena — one rendered document each — and each owns its own style engine,
//! layout engine, session history and set of execution worlds.
//!
//! Why one arena rather than one per frame: `enter_active_tree` refuses to
//! nest a *different* tree, and parent hit testing and parent paint must
//! descend into a child document on the same stack. Separate arenas would also
//! re-issue the same `NodeId` generations, so an id from one frame would alias
//! a node in another — the one failure the generation checks exist to prevent.
//! ADR-0035 D1 has the full argument.
//!
//! [`FrameId`]s are generation-checked for the same reason `NodeId`s are: a
//! frame id outlives its frame. It is stored in listener keys, in each world's
//! state and in the CDP frame registry, so a detached frame whose slot was
//! reused would silently make a stale id name an unrelated browsing context.
//!
//! Only the top-level context exists so far. Attaching and detaching nested
//! ones — with the frame caps, the owner-element mapping and the per-frame
//! loading state — arrives with `<iframe>` loading.

use std::cell::{Cell, RefCell};
use std::num::NonZeroU32;
use std::rc::Rc;

use oxidepage_base::id::FIRST_GENERATION;
use oxidepage_base::{FrameId, NodeId};
use oxidepage_bindings::FrameShared;
use oxidepage_dom::DomTree;

/// One browsing context.
pub(crate) struct Frame {
    id: FrameId,
    /// The embedding context, or `None` for the top-level frame.
    parent: Option<FrameId>,
    /// This context's bindings state, which carries its rendered document and
    /// its style and layout engines.
    shared: Rc<FrameShared>,
}

impl Frame {
    pub(crate) fn shared(&self) -> &Rc<FrameShared> {
        &self.shared
    }

    /// This context's rendered document.
    pub(crate) fn document(&self) -> NodeId {
        self.shared.document()
    }
}

/// A generation-checked slot in the [`FrameTree`].
struct Slot {
    /// The generation this slot issues ids at. Bumped when the slot is freed,
    /// so an id naming the previous occupant fails its check rather than
    /// addressing the new one.
    generation: NonZeroU32,
    frame: Option<Rc<Frame>>,
}

/// Every live browsing context of one page, the top-level one first.
pub(crate) struct FrameTree {
    slots: RefCell<Vec<Slot>>,
    /// The top-level context. Never freed while the page lives.
    main: Cell<FrameId>,
}

impl FrameTree {
    /// Builds the tree around its top-level context.
    pub(crate) fn new(shared: Rc<FrameShared>) -> Self {
        let id = FrameId::from_parts(0, FIRST_GENERATION);
        let main = Frame {
            id,
            parent: None,
            shared,
        };
        Self {
            slots: RefCell::new(vec![Slot {
                generation: FIRST_GENERATION,
                frame: Some(Rc::new(main)),
            }]),
            main: Cell::new(id),
        }
    }

    /// The frame `id` names, or `None` if it has been detached.
    pub(crate) fn get(&self, id: FrameId) -> Option<Rc<Frame>> {
        let slots = self.slots.borrow();
        let slot = slots.get(id.index() as usize)?;
        if slot.generation != id.generation() {
            return None;
        }
        slot.frame.clone()
    }

    /// Live frames, **parents before their children**.
    ///
    /// This is the order reflow and paint need: a parent's layout fixes the
    /// content box its child lays out into, and an `<iframe>` is sized as a
    /// replaced element, so a child never feeds back (ADR-0035 D6).
    pub(crate) fn pre_order(&self) -> Vec<Rc<Frame>> {
        let mut out = Vec::new();
        self.walk_from(self.main.get(), &mut out);
        out
    }

    fn walk_from(&self, id: FrameId, out: &mut Vec<Rc<Frame>>) {
        let Some(frame) = self.get(id) else { return };
        out.push(frame);
        for child in self.children(id) {
            self.walk_from(child.id, out);
        }
    }

    /// The frames embedded directly in `parent`, in creation order.
    fn children(&self, parent: FrameId) -> Vec<Rc<Frame>> {
        self.slots
            .borrow()
            .iter()
            .filter_map(|slot| slot.frame.clone())
            .filter(|frame| frame.parent == Some(parent))
            .collect()
    }

    /// The frame rendering `doc`, if any.
    pub(crate) fn of_document(&self, doc: NodeId) -> Option<Rc<Frame>> {
        self.slots
            .borrow()
            .iter()
            .filter_map(|slot| slot.frame.clone())
            .find(|frame| frame.document() == doc)
    }

    /// The frame `node` belongs to, via its node document.
    ///
    /// This is how a queue entry — a style update, an image update — finds the
    /// engine that must consume it: the arena is shared, so the node itself
    /// carries no frame.
    ///
    /// `None` for a node that has been freed since it was queued (the queues
    /// hold snapshots, L3) and for one in a document with no browsing context.
    ///
    /// `containing_document`, not `node_document`: a node inside a shadow tree
    /// is owned by its shadow root, and a `<style>` there belongs to the frame
    /// its host is in.
    pub(crate) fn of_node(&self, dom: &DomTree, node: NodeId) -> Option<Rc<Frame>> {
        self.of_document(dom.containing_document(node)?)
    }
}
