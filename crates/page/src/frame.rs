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
//! A nested context is created when its `<iframe>` enters a rendered document
//! and discarded when it leaves — HTML does this independently of `src`, so an
//! `<iframe>` with no `src` still owns a real `about:blank` document.

use std::cell::{Cell, RefCell};
use std::num::NonZeroU32;
use std::rc::Rc;

use oxidepage_base::id::FIRST_GENERATION;
use oxidepage_base::{FrameId, NodeId};
use oxidepage_bindings::FrameShared;
use oxidepage_dom::DomTree;

/// Deepest nesting of browsing contexts.
///
/// HTML sets no limit; every browser imposes one. Ours bounds a *page* rather
/// than a driver: unlike a world, page script creates frames freely, so this
/// and [`MAX_FRAMES_PER_PAGE`] are what keep `<iframe src>` recursion from
/// being a host-exhaustion primitive (ADR-0035 D3).
pub(crate) const MAX_FRAME_DEPTH: usize = 10;

/// Most live browsing contexts one page may hold, the top-level one included.
///
/// Each costs a style engine, a layout engine and at least one whole
/// `rquickjs::Runtime`, so the JS memory ceiling is `frames × worlds ×
/// memory_limit` — the same reasoning as ADR-0027's `max_pages_per_context`,
/// one level down.
pub(crate) const MAX_FRAMES_PER_PAGE: usize = 64;

/// One browsing context.
pub(crate) struct Frame {
    id: FrameId,
    /// The embedding context, or `None` for the top-level frame.
    parent: Option<FrameId>,
    /// The `<iframe>` element that owns this context, in the *parent's*
    /// document. `None` only for the top-level frame.
    owner: Option<NodeId>,
    /// This context's bindings state, which carries its rendered document and
    /// its style and layout engines.
    shared: Rc<FrameShared>,
}

impl Frame {
    pub(crate) fn id(&self) -> FrameId {
        self.id
    }

    /// The `<iframe>` element embedding this context, in its parent's document.
    pub(crate) fn owner(&self) -> Option<NodeId> {
        self.owner
    }

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
    /// The slot's generation has run out and it will never be reused: reusing
    /// it would re-issue an id its last occupant already handed out. The node
    /// arena retires slots for exactly this reason.
    retired: bool,
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
            owner: None,
            shared,
        };
        Self {
            slots: RefCell::new(vec![Slot {
                generation: FIRST_GENERATION,
                retired: false,
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

    /// The frame the `<iframe>` element `owner` embeds.
    pub(crate) fn of_owner(&self, owner: NodeId) -> Option<Rc<Frame>> {
        self.slots
            .borrow()
            .iter()
            .filter_map(|slot| slot.frame.clone())
            .find(|frame| frame.owner == Some(owner))
    }

    /// How deep `id` sits, the top-level context being 0.
    fn depth(&self, id: FrameId) -> usize {
        let mut depth = 0;
        let mut current = self.get(id).and_then(|frame| frame.parent);
        while let Some(parent) = current {
            depth += 1;
            if depth > MAX_FRAME_DEPTH {
                break;
            }
            current = self.get(parent).and_then(|frame| frame.parent);
        }
        depth
    }

    /// Live frame count, the top-level context included.
    pub(crate) fn len(&self) -> usize {
        self.slots
            .borrow()
            .iter()
            .filter(|slot| slot.frame.is_some())
            .count()
    }

    /// Whether a context nested in `parent` would fit under both caps.
    ///
    /// Asked *before* the caller builds the engines and the realm, so a refusal
    /// costs nothing.
    pub(crate) fn has_room_under(&self, parent: FrameId) -> bool {
        self.len() < MAX_FRAMES_PER_PAGE && self.depth(parent) < MAX_FRAME_DEPTH
    }

    /// Registers a nested browsing context owned by `owner` in `parent`.
    ///
    /// `build` receives the id the slot allocated, because a `FrameShared`
    /// carries its own frame id: minting the id outside and trusting the two to
    /// agree is a silent mismatch waiting to happen.
    pub(crate) fn attach(
        &self,
        parent: FrameId,
        owner: NodeId,
        build: impl FnOnce(FrameId) -> Rc<FrameShared>,
    ) -> Rc<Frame> {
        let mut slots = self.slots.borrow_mut();
        let free = slots
            .iter()
            .position(|slot| slot.frame.is_none() && !slot.retired);
        let (index, generation) = match free {
            Some(index) => (index, slots[index].generation),
            None => {
                slots.push(Slot {
                    generation: FIRST_GENERATION,
                    retired: false,
                    frame: None,
                });
                (slots.len() - 1, FIRST_GENERATION)
            }
        };
        let id = FrameId::from_parts(
            u32::try_from(index).expect("frame count is capped far below u32::MAX"),
            generation,
        );
        let frame = Rc::new(Frame {
            id,
            parent: Some(parent),
            owner: Some(owner),
            shared: build(id),
        });
        slots[index].frame = Some(Rc::clone(&frame));
        frame
    }

    /// Retires a nested context and every context beneath it, **deepest
    /// first**, and reports them so the caller can tear each one down.
    ///
    /// Deepest first because teardown is ordered: a child's realm and document
    /// must go before its parent's, exactly as `WorldTable::teardown` releases
    /// the newest world first.
    ///
    /// The top-level frame is never detached: it *is* the page.
    pub(crate) fn detach(&self, id: FrameId) -> Vec<Rc<Frame>> {
        if id == self.main.get() {
            return Vec::new();
        }
        let mut doomed = Vec::new();
        self.walk_from(id, &mut doomed);
        doomed.reverse();
        let mut slots = self.slots.borrow_mut();
        for frame in &doomed {
            let Some(slot) = slots.get_mut(frame.id.index() as usize) else {
                continue;
            };
            slot.frame = None;
            match slot.generation.checked_add(1) {
                Some(next) => slot.generation = next,
                None => slot.retired = true,
            }
        }
        doomed
    }

    /// Retires **every** nested context, deepest first, leaving the top-level
    /// frame alone.
    ///
    /// What a top-level navigation does: the outgoing document's `<iframe>`
    /// elements are gone with it, so every context they owned is gone too. The
    /// arena is *replaced* rather than mutated at a commit, so no disconnection
    /// is ever queued for them — without this they would stay in the table with
    /// a document id naming a freed slot, and the next whole-page walk would
    /// panic on it.
    pub(crate) fn detach_nested(&self) -> Vec<Rc<Frame>> {
        let children: Vec<FrameId> = self
            .pre_order()
            .into_iter()
            .filter(|frame| frame.id() != self.main.get())
            .map(|frame| frame.id())
            .collect();
        let mut doomed = Vec::new();
        for id in children {
            // Each call takes the subtree beneath it too, so a frame already
            // retired by an earlier one answers with nothing.
            doomed.extend(self.detach(id));
        }
        doomed
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
