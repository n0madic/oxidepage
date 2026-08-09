//! The page's execution worlds (ADR-0033).
//!
//! A world is a whole [`QuickJsRealm`] — its own `rquickjs::Runtime` *and*
//! `Context` — not a second context on a shared runtime. Two properties of
//! rquickjs decided that, and both are load-bearing enough to repeat here:
//!
//! - `Context::with` takes `RefCell::borrow_mut` on the runtime, so on a shared
//!   runtime entering world B from inside world A's scope is a `BorrowMutError`
//!   — and that nesting is exactly what synchronous cross-world event delivery
//!   is.
//! - `Persistent::restore` compares only the **runtime** pointer, so on a
//!   shared runtime a world-A value would silently restore into world B. One
//!   runtime per world turns the cross-world wrapper leak into a typed error at
//!   the first touch, and every existing brand check rejects a foreign object
//!   with its existing `TypeError` for free.
//!
//! The costs are bounded here: [`MAX_WORLDS`] caps the install cost a driver
//! can ask for, [`MAX_WORLD_DEPTH`] caps nested cross-world delivery, and
//! [`WORLD_STACK_BYTES`] makes each world's native-stack budget ours rather
//! than QuickJS's 1 MiB default.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxidepage_base::FrameId;
use oxidepage_bindings::{BindCx, WorldEnter, WorldId, WorldState};
use oxidepage_js::{JsRealm, QuickJsRealm};

/// Most worlds one page may hold, main world included.
///
/// Creating a world runs `bootstrap.js`, seeds the CSS property map and
/// registers ~150 interfaces, and that cost is paid again for every registered
/// world at every navigation. Page script cannot create a world — only driver
/// commands can — so this bounds a buggy driver rather than a hostile page.
pub(crate) const MAX_WORLDS: usize = 32;

/// Deepest chain of nested cross-world deliveries.
///
/// World A dispatches, B's listener dispatches, delivery re-enters A… Each hop
/// costs a native stack frame chain plus a world's JS budget, so the recursion
/// is capped rather than left to overflow the page thread's stack.
pub(crate) const MAX_WORLD_DEPTH: u32 = 8;

/// Native-stack budget per world.
///
/// Explicit, because QuickJS's default is 1 MiB against a page thread that must
/// hold [`MAX_WORLD_DEPTH`] of them; `QuickJsRealm::anchor_stack` is what makes
/// this measured from each world's own entry point rather than from wherever it
/// happened to be created.
pub(crate) const WORLD_STACK_BYTES: usize = 512 * 1024;

/// One execution world: a realm, its bindings state, and its re-entry latch.
pub(crate) struct World {
    pub(crate) id: WorldId,
    /// The browsing context this world belongs to. A world is per (frame,
    /// world name), so the same isolated world in two frames is two realms
    /// (ADR-0035 D3).
    pub(crate) frame: FrameId,
    /// `""` for the main world. A world's name is its only identity over CDP.
    pub(crate) name: RefCell<String>,
    /// True while this world is on the stack. Entering a `Context` that is
    /// already entered is a `BorrowMutError` inside rquickjs, so the delivery
    /// is refused here instead (ADR-0033 D4).
    entered: Cell<bool>,
    /// Drop order within a world: `state` owns this world's `Persistent`s and
    /// must drop **before** `realm` frees the runtime, or `JS_FreeRuntime`
    /// aborts the process on a non-empty `gc_obj_list`.
    state: RefCell<Option<Rc<WorldState>>>,
    realm: QuickJsRealm,
}

impl World {
    /// This world's bindings state, or `None` once it has been torn down.
    pub(crate) fn state(&self) -> Option<Rc<WorldState>> {
        self.state.borrow().clone()
    }

    pub(crate) fn realm(&self) -> &QuickJsRealm {
        &self.realm
    }
}

/// Every world of one page, main world at index 0.
///
/// Owned only by [`crate::Page`], which is what makes the teardown in
/// [`WorldTable::teardown`] deterministic: no other strong reference can keep a
/// realm alive past the page's own drop.
pub(crate) struct WorldTable {
    slots: RefCell<Vec<Rc<World>>>,
    depth: Cell<u32>,
}

impl WorldTable {
    pub(crate) fn new() -> Self {
        Self {
            slots: RefCell::new(Vec::new()),
            depth: Cell::new(0),
        }
    }

    /// Registers an installed world. The caller has already built the realm and
    /// run `install_world` over it.
    pub(crate) fn push(
        &self,
        id: WorldId,
        frame: FrameId,
        name: &str,
        realm: QuickJsRealm,
        state: Rc<WorldState>,
    ) {
        self.slots.borrow_mut().push(Rc::new(World {
            id,
            frame,
            name: RefCell::new(name.to_owned()),
            entered: Cell::new(false),
            state: RefCell::new(Some(state)),
            realm,
        }));
    }

    /// How many live worlds `frame` holds.
    pub(crate) fn count_in(&self, frame: FrameId) -> usize {
        self.slots
            .borrow()
            .iter()
            .filter(|w| w.frame == frame)
            .count()
    }

    pub(crate) fn get(&self, id: WorldId) -> Option<Rc<World>> {
        self.slots.borrow().iter().find(|w| w.id == id).cloned()
    }

    /// The world called `name` **in `frame`**.
    ///
    /// Scoped, because a driver's `Page.createIsolatedWorld` names a world per
    /// frame: the same `worldName` in two frames is two realms, and an
    /// unscoped lookup would hand back whichever was created first.
    pub(crate) fn by_name(&self, frame: FrameId, name: &str) -> Option<Rc<World>> {
        self.slots
            .borrow()
            .iter()
            .find(|w| w.frame == frame && *w.name.borrow() == name)
            .cloned()
    }

    /// Whether `world` is currently on the stack.
    pub(crate) fn is_entered(&self, world: WorldId) -> bool {
        self.get(world).is_some_and(|w| w.entered.get())
    }

    /// Every live world, main first then creation order.
    pub(crate) fn all(&self) -> Vec<Rc<World>> {
        self.slots.borrow().clone()
    }

    /// The lowest unused world id. Ids are dense and **are** reused when a
    /// world is rebuilt at a commit — which is exactly why a `WorldId` never
    /// crosses the thread boundary and the monotonic `context_id` does.
    pub(crate) fn next_id(&self) -> WorldId {
        self.slots
            .borrow()
            .iter()
            .map(|w| w.id + 1)
            .max()
            .unwrap_or(0)
    }

    /// Tears down every isolated world, leaving the main world alone.
    ///
    /// Called at each commit: a `worldName` init script must run against a
    /// fresh global, and a rebuilt world's wrapper cache, slab, listeners and
    /// object store would otherwise all name the dead document. Returns the
    /// names to rebuild, in creation order.
    pub(crate) fn take_isolated(&self) -> Vec<(FrameId, String)> {
        let mut names = Vec::new();
        let drained: Vec<Rc<World>> = {
            let mut slots = self.slots.borrow_mut();
            // Every frame's *default* world is kept, not just `MAIN_WORLD`:
            // that constant is only the top-level frame's (ADR-0035 D3), and
            // dropping a nested frame's default world here would leave it with
            // no `window` at all.
            let (keep, go): (Vec<_>, Vec<_>) =
                slots.drain(..).partition(|w| w.name.borrow().is_empty());
            *slots = keep;
            go
        };
        for world in &drained {
            // The frame it belonged to, so the rebuild puts it back there
            // rather than on the main frame.
            names.push((world.frame, world.name.borrow().clone()));
        }
        for world in drained.iter().rev() {
            Self::destroy(world);
        }
        drop(drained);
        names
    }

    /// Releases one world's JS values while its runtime is still alive, then
    /// lets the realm go.
    fn destroy(world: &Rc<World>) {
        let state = world.state.borrow_mut().take();
        if let Some(state) = state {
            // Empty the containers rather than rely on this being the last
            // strong reference: `Page` keeps its own `Rc` for the main world
            // and the realm holds a third as `Rc<dyn Any>`, so dropping one
            // handle frees nothing. What matters is that every `Persistent` is
            // released *now*, while `world.realm` is still alive.
            state.release_js();
            // Its cursor must not hold the connectivity log back once the
            // world is gone.
            state.frame.global.forget_world_cursor(world.id);
            // Its handles are gone with its store; the page index must not keep
            // routing an `objectId` at a world that no longer exists.
            state.frame.global.forget_objects_of(world.id);
        }
    }

    /// Releases every world of `frame` and hands back their realms, still
    /// alive, for the caller to drop.
    ///
    /// The realms come back rather than being dropped here because a page
    /// detaching a subtree of frames must release **all** their values before
    /// freeing **any** runtime — the same two-pass rule [`Self::teardown`]
    /// documents, and the same process abort if it is collapsed into one.
    #[must_use]
    pub(crate) fn release_frame(&self, frame: FrameId) -> Vec<Rc<World>> {
        let doomed: Vec<Rc<World>> = {
            let mut slots = self.slots.borrow_mut();
            let (doomed, kept) = slots.drain(..).partition(|w| w.frame == frame);
            *slots = kept;
            doomed
        };
        // Newest world first, mirroring `teardown`.
        for world in doomed.iter().rev() {
            Self::destroy(world);
        }
        doomed
    }

    /// Destroys every world.
    ///
    /// **Two passes, and the split is load-bearing.** Every world's JS values
    /// are released first, and only then may any realm be freed: a value can
    /// legitimately be filed in another world's container mid-teardown (a
    /// remote handle, a shared event payload), and freeing runtime A while
    /// world B still holds one of A's `Persistent`s is the `JS_FreeRuntime`
    /// abort. Releasing world-by-world and dropping each realm as it goes —
    /// which is what a single `for` over the drained `Rc<World>`s does, since
    /// the binding drops at the end of each iteration — reintroduces exactly
    /// that. `dropping_a_page_with_live_worlds_is_clean` catches it.
    ///
    /// Explicit rather than left to field order because the ordering is spread
    /// across `Page`, `WorldTable` and `World`, and getting it wrong is a
    /// process abort rather than a test failure.
    pub(crate) fn teardown(&self) {
        let drained: Vec<Rc<World>> = self.slots.borrow_mut().drain(..).collect();
        // Pass 1: every world's values, newest world first, every runtime alive.
        for world in drained.iter().rev() {
            Self::destroy(world);
        }
        // Pass 2: the realms, once nothing holds a `Persistent` any more.
        drop(drained);
    }
}

impl Drop for WorldTable {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Clears `entered` even if the callback unwinds, so one panicking listener
/// does not wedge a world shut for the rest of the page's life.
pub(crate) struct EnterGuard<'a> {
    world: Rc<World>,
    table: &'a WorldTable,
}

impl Drop for EnterGuard<'_> {
    fn drop(&mut self) {
        self.world.entered.set(false);
        self.table
            .depth
            .set(self.table.depth.get().saturating_sub(1));
    }
}

impl WorldTable {
    /// Marks `world` as being on the stack until the guard drops.
    ///
    /// `None` when it is already entered or the nesting cap is hit — the two
    /// cases a caller must refuse rather than push through, because entering a
    /// live `Context` is a `BorrowMutError` inside rquickjs.
    ///
    /// **Every** entry into a realm has to take this, the main world's
    /// included. `Page::with_cx` is the main world's entry and does *not* go
    /// through `WorldEnter::enter`, so without arming the latch there a
    /// cross-world delivery arriving back into main while main's own scope is
    /// live re-enters a borrowed `Context` and panics — which kills the page
    /// thread, and is reachable from ordinary page script.
    pub(crate) fn mark_entered(&self, world: WorldId) -> Option<EnterGuard<'_>> {
        let world = self.get(world)?;
        if world.entered.get() || self.depth.get() >= MAX_WORLD_DEPTH {
            return None;
        }
        world.entered.set(true);
        self.depth.set(self.depth.get() + 1);
        Some(EnterGuard { world, table: self })
    }
}

impl WorldEnter for WorldTable {
    fn enter(&self, world: WorldId, f: &mut dyn FnMut(&BindCx<'_>)) -> bool {
        let Some(world) = self.get(world) else {
            return false;
        };
        let Some(state) = world.state() else {
            return false;
        };
        // Refused, not queued: re-entering a live `Context` panics inside
        // rquickjs, and there is no correct place to defer a synchronous
        // delivery to (ADR-0033 D4 records this as the sharpest cost of one
        // runtime per world).
        let Some(_guard) = self.mark_entered(world.id) else {
            return false;
        };
        world.realm.with_scope(|scope| {
            let cx = BindCx {
                scope,
                state: Rc::clone(&state),
            };
            f(&cx);
        });
        true
    }

    fn world_ids_of(&self, frame: FrameId) -> Vec<WorldId> {
        self.slots
            .borrow()
            .iter()
            .filter(|w| w.frame == frame)
            .map(|w| w.id)
            .collect()
    }

    fn has_listener(
        &self,
        world: WorldId,
        target: oxidepage_bindings::EventTargetKey,
        event_type: &str,
    ) -> bool {
        self.get(world)
            .and_then(|w| w.state())
            .is_some_and(|state| state.has_listener(target, event_type))
    }
}
