//! Scroll offsets: real, clamped offsets per scroll container plus the
//! viewport scroll (ADR-0006 §8 — no scrollbars, no smooth scrolling;
//! `scroll` events are dispatched by the page from [`ScrollResult`]).

use std::collections::HashMap;

use oxidepage_base::{NodeId, Point};

/// Scroll positions live on the [`LayoutEngine`](crate::LayoutEngine), not
/// the box tree, so they survive full rebuilds. Offsets are clamped on
/// write and re-clamped on read against the current layout.
#[derive(Default)]
pub struct ScrollState {
    /// Per-element scroll offsets (only scroll containers get entries).
    pub(crate) offsets: HashMap<NodeId, Point>,
    /// The viewport (document) scroll offset.
    pub(crate) viewport: Point,
    /// Monotonic counter bumped whenever an *element* overflow scroll actually
    /// changes. Element scroll is baked into item origins at paint time (design
    /// doc §5.11), so the display-list cache keys on this.
    pub(crate) element_version: u64,
    /// Monotonic counter bumped whenever the *document* (viewport) scroll
    /// actually changes. Unlike element scroll, document scroll is applied by
    /// the rasterizer rather than baked into the display list, so it is
    /// deliberately absent from the paint stamp — the cached list is reused
    /// across document scroll positions.
    pub(crate) document_version: u64,
}

impl ScrollState {
    /// Bumps [`Self::element_version`] after an element overflow scroll changed.
    pub(crate) fn note_element_changed(&mut self) {
        self.element_version += 1;
    }

    /// Bumps [`Self::document_version`] after the document scroll changed.
    pub(crate) fn note_document_changed(&mut self) {
        self.document_version += 1;
    }
}

/// Result of a scroll write: the clamped position and whether it changed
/// (a change means the page must queue a `scroll` event for the target).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollResult {
    pub x: f32,
    pub y: f32,
    pub changed: bool,
}
