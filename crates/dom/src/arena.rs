//! Generational arena storing the document's nodes.
//!
//! Slots are `Vec`-backed for cache-friendly traversal; freed slots go on a
//! free list and bump their generation, so a stale [`NodeId`] can never alias
//! a reused slot — it fails its generation check instead (design doc §5.2).

use std::num::NonZeroU32;

use oxidepage_base::NodeId;
use oxidepage_base::id::FIRST_GENERATION;

use crate::node::Node;

struct Slot {
    generation: NonZeroU32,
    node: Option<Node>,
}

/// Arena of [`Node`]s with generation-checked access.
pub struct Arena {
    slots: Vec<Slot>,
    free: Vec<u32>,
    live: usize,
    /// Generation handed to a *fresh* slot. Normally [`FIRST_GENERATION`], but
    /// a navigation seeds the next arena above the outgoing one's high-water
    /// mark so that ids of the old document cannot alias the new document's
    /// nodes (see [`Arena::with_generation_base`]).
    generation_base: NonZeroU32,
}

impl Default for Arena {
    fn default() -> Self {
        Self::with_generation_base(FIRST_GENERATION)
    }
}

impl Arena {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an arena whose fresh slots start at `base` rather than
    /// [`FIRST_GENERATION`].
    ///
    /// Navigation throws the whole arena away, so without this the next arena
    /// would re-issue `(k, FIRST_GENERATION)` and every id the previous
    /// document handed to script would silently *alias* an unrelated node
    /// instead of going stale. Seeding above the outgoing arena's high-water
    /// mark ([`Arena::next_generation_base`]) retires them all at once.
    ///
    /// **Slot 0 is the deliberate exception**: it always gets
    /// [`FIRST_GENERATION`]. The document is allocated first into an empty
    /// arena, and `window.document` is a non-configurable data property whose
    /// wrapper outlives navigation (the realm does), so its payload must keep
    /// resolving — to the *new* document, which is the specified behaviour.
    #[must_use]
    pub fn with_generation_base(base: NonZeroU32) -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            live: 0,
            generation_base: base,
        }
    }

    /// The generation a successor arena must start from so that no id of this
    /// arena survives into it.
    ///
    /// Saturates at `u32::MAX`: exhausting the generation space would take
    /// ~2^32 navigations, and there is nowhere left to go — the same cliff
    /// [`Arena::free`] handles by retiring a slot.
    #[must_use]
    pub fn next_generation_base(&self) -> NonZeroU32 {
        let high_water = self
            .slots
            .iter()
            .map(|slot| slot.generation)
            .max()
            .unwrap_or(self.generation_base)
            .max(self.generation_base);
        high_water.checked_add(1).unwrap_or(NonZeroU32::MAX)
    }

    /// Number of live nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Allocates a slot for `node` and returns its id.
    pub fn alloc(&mut self, node: Node) -> NodeId {
        self.live += 1;
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            debug_assert!(slot.node.is_none(), "free list pointed at a live slot");
            slot.node = Some(node);
            return NodeId::from_parts(index, slot.generation);
        }
        let index = u32::try_from(self.slots.len()).expect("arena exceeded u32::MAX slots");
        // Slot 0 — the document — keeps `FIRST_GENERATION` across navigations
        // so the pinned `document` wrapper resolves to the incoming document.
        let generation = if index == 0 {
            FIRST_GENERATION
        } else {
            self.generation_base
        };
        self.slots.push(Slot {
            generation,
            node: Some(node),
        });
        NodeId::from_parts(index, generation)
    }

    /// Frees the slot behind `id`. Returns the node if `id` was live.
    ///
    /// Freeing bumps the slot's generation, invalidating every outstanding
    /// copy of `id`.
    pub fn free(&mut self, id: NodeId) -> Option<Node> {
        let slot = self.slots.get_mut(id.index() as usize)?;
        if slot.generation != id.generation() || slot.node.is_none() {
            return None;
        }
        let node = slot.node.take();
        // Saturating: a slot whose generation would wrap is retired rather
        // than recycled, preserving the no-aliasing guarantee.
        if let Some(next) = slot.generation.checked_add(1) {
            slot.generation = next;
            self.free.push(id.index());
        }
        self.live -= 1;
        node
    }

    /// True if `id` refers to a live node.
    #[must_use]
    pub fn contains(&self, id: NodeId) -> bool {
        self.get(id).is_some()
    }

    #[must_use]
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        let slot = self.slots.get(id.index() as usize)?;
        if slot.generation != id.generation() {
            return None;
        }
        slot.node.as_ref()
    }

    #[must_use]
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        let slot = self.slots.get_mut(id.index() as usize)?;
        if slot.generation != id.generation() {
            return None;
        }
        slot.node.as_mut()
    }

    /// Panicking accessor for internal use where `id` is known-live.
    #[must_use]
    pub fn node(&self, id: NodeId) -> &Node {
        self.get(id).expect("stale NodeId")
    }

    /// Panicking mutable accessor for internal use where `id` is known-live.
    #[must_use]
    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        self.get_mut(id).expect("stale NodeId")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeData;

    fn text_node(s: &str) -> Node {
        Node::new(NodeData::Text(s.into()))
    }

    #[test]
    fn alloc_get_free_roundtrip() {
        let mut arena = Arena::new();
        let a = arena.alloc(text_node("a"));
        let b = arena.alloc(text_node("b"));
        assert_eq!(arena.len(), 2);
        assert!(arena.contains(a));
        assert!(arena.get(b).is_some());

        arena.free(a);
        assert!(!arena.contains(a));
        assert_eq!(arena.len(), 1);
    }

    #[test]
    fn successor_arena_retires_every_id_but_the_document() {
        let mut old = Arena::new();
        let doc = old.alloc(text_node("doc"));
        let a = old.alloc(text_node("a"));
        let b = old.alloc(text_node("b"));
        old.free(b); // bumps a generation above the base
        let c = old.alloc(text_node("c"));

        let mut new = Arena::with_generation_base(old.next_generation_base());
        let new_doc = new.alloc(text_node("doc2"));
        for _ in 0..8 {
            new.alloc(text_node("filler"));
        }

        // The document keeps its identity, by design.
        assert_eq!(new_doc, doc);
        assert!(new.contains(new_doc));
        // Every other id of the old arena is stale in the new one.
        for stale in [a, b, c] {
            assert!(new.get(stale).is_none(), "{stale:?} aliased a fresh slot");
        }
    }

    #[test]
    fn generation_base_saturates_instead_of_wrapping() {
        let arena = Arena::with_generation_base(NonZeroU32::MAX);
        assert_eq!(arena.next_generation_base(), NonZeroU32::MAX);
    }

    #[test]
    fn stale_id_does_not_alias_reused_slot() {
        let mut arena = Arena::new();
        let a = arena.alloc(text_node("a"));
        arena.free(a);
        let b = arena.alloc(text_node("b"));
        // Slot is reused, but the stale id fails its generation check.
        assert_eq!(a.index(), b.index());
        assert!(arena.get(a).is_none());
        assert!(arena.get(b).is_some());
        assert!(arena.free(a).is_none());
    }
}
