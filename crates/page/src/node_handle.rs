//! Stable, wire-safe names for DOM nodes (ADR-0031 D1).
//!
//! A driver needs to name a node in a JSON message and hand the same name back
//! later — CDP calls it `backendNodeId`. The obvious encoding, packing
//! [`NodeId`]'s `{index, generation}` into one integer, **cannot** be used:
//! CDP node ids are JSON numbers, exact only below 2^53, and the generation
//! genuinely uses its full `u32` range (`Arena::free` bumps it, and every
//! navigation seeds the fresh arena above the outgoing one's high-water mark).
//! `generation << 32 | index` therefore rounds away the *low* bits — the index
//! — and a corrupted token would name a **different live node** with no error
//! anywhere. That is strictly worse than not carrying the generation at all.
//!
//! So the mapping is a table, which carries the generation literally: a handle
//! is an opaque counter, and the `NodeId` behind it keeps its own generation.
//! Resolving goes through `Arena::get`, so a handle to a freed node fails the
//! generation check for free.
//!
//! The store deliberately does **not** pin its nodes. A handle naming a
//! collected node must fail; pinning would turn a driver's node cache into a
//! document-lifetime leak.

use std::collections::HashMap;

use oxidepage_base::NodeId;

/// Most handles one page hands out before it starts refusing.
///
/// Sized against `DOM.getDocument { depth: -1 }` on a genuinely large document:
/// a heavy real-world page is tens of thousands of nodes, so this is roughly an
/// order of magnitude of headroom over the largest single call a driver makes.
/// Past it, [`NodeHandleStore::intern`] reports failure rather than growing
/// without bound — the same posture as
/// [`MAX_REMOTE_OBJECTS`](oxidepage_bindings::remote::MAX_REMOTE_OBJECTS).
pub const MAX_NODE_HANDLES: usize = 100_000;

/// `backendNodeId` ↔ [`NodeId`], both ways.
#[derive(Default)]
pub struct NodeHandleStore {
    /// Monotonic from 1; never recycled, so a stale handle names nothing rather
    /// than whatever was interned next. `0` is reserved for "no match", which is
    /// how `DOM.querySelector` spells a miss.
    next: u64,
    by_handle: HashMap<u64, NodeId>,
    /// One stable handle per node — **load-bearing, not an optimization**.
    /// Puppeteer's `bindIsolatedHandle` decorator round-trips through
    /// `DOM.describeNode` on nearly every `ElementHandle` call, so without this
    /// the table would grow per *call* instead of per *distinct node*.
    by_node: HashMap<NodeId, u64>,
}

impl NodeHandleStore {
    /// The handle for `node`, minting one on first sight.
    ///
    /// `None` once [`MAX_NODE_HANDLES`] handles are live; the caller sweeps
    /// dead entries and retries before reporting that outward.
    pub fn intern(&mut self, node: NodeId) -> Option<u64> {
        if let Some(&handle) = self.by_node.get(&node) {
            return Some(handle);
        }
        if self.by_handle.len() >= MAX_NODE_HANDLES {
            return None;
        }
        self.next += 1;
        let handle = self.next;
        self.by_handle.insert(handle, node);
        self.by_node.insert(node, handle);
        Some(handle)
    }

    /// The node a handle names, if it ever named one. Liveness is *not* checked
    /// here — the caller re-validates through the arena, which is the only
    /// place the generation can be checked.
    #[must_use]
    pub fn get(&self, handle: u64) -> Option<NodeId> {
        self.by_handle.get(&handle).copied()
    }

    /// Drops every entry whose node `live` rejects.
    ///
    /// A dead node's handle could not be resolved anyway, so forgetting it
    /// loses nothing — which is what makes sweeping a safe answer to a full
    /// table rather than a heuristic.
    pub fn retain(&mut self, live: impl Fn(NodeId) -> bool) {
        self.by_handle.retain(|_, node| live(*node));
        let kept = &self.by_handle;
        self.by_node.retain(|_, handle| kept.contains_key(handle));
    }

    /// Forgets every handle. Navigation does this: each one named a node of the
    /// outgoing document.
    pub fn clear(&mut self) {
        self.by_handle.clear();
        self.by_node.clear();
    }

    /// How many handles are live. Diagnostic, and what the tests assert on.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_handle.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_handle.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(index: u32, generation: u32) -> NodeId {
        NodeId::from_parts(
            index,
            std::num::NonZeroU32::new(generation).expect("a non-zero generation"),
        )
    }

    #[test]
    fn one_node_gets_one_handle_and_a_freed_slot_gets_a_fresh_one() {
        let mut store = NodeHandleStore::default();
        let a = node(1, 1);
        assert_eq!(store.intern(a), store.intern(a), "stable per node");
        // The same arena *index* at a later generation is a different node, and
        // must not inherit the handle.
        assert_ne!(store.intern(node(1, 2)), store.intern(a));
        let handle = store.intern(a).expect("a handle");
        assert_eq!(store.get(handle), Some(a));
        assert_eq!(store.get(0), None, "0 is reserved for `no match`");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn a_full_table_refuses_rather_than_growing() {
        let mut store = NodeHandleStore::default();
        for index in 0..u32::try_from(MAX_NODE_HANDLES).unwrap() {
            assert!(store.intern(node(index, 1)).is_some());
        }
        assert_eq!(store.len(), MAX_NODE_HANDLES);
        // Past the cap a *new* node is refused — the caller sweeps and reports
        // `OutOfHandles` rather than emitting handle 0, which would name nothing.
        assert_eq!(store.intern(node(u32::MAX, 1)), None);
        // A node already in the table is still served.
        assert!(store.intern(node(0, 1)).is_some());
    }

    #[test]
    fn a_sweep_drops_dead_entries_and_never_reissues_a_handle() {
        let mut store = NodeHandleStore::default();
        let live = node(1, 1);
        let dead = node(2, 1);
        let live_handle = store.intern(live).unwrap();
        let dead_handle = store.intern(dead).unwrap();

        store.retain(|id| id == live);
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(live_handle), Some(live));
        assert_eq!(store.get(dead_handle), None);

        // Counter is monotonic across a sweep and across a clear: a handle a
        // driver still holds must name nothing, never something new.
        let next = store.intern(node(3, 1)).unwrap();
        assert!(next > dead_handle);
        store.clear();
        assert!(store.is_empty());
        assert!(store.intern(node(4, 1)).unwrap() > next);
    }
}
