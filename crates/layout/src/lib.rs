//! Box tree, taffy driver, parley inline formatting contexts, and geometry
//! queries (design doc §10, Phase 5, ADR-0006).
//!
//! Layout algorithms are adapted from `blitz-dom` 0.3.0-alpha.6
//! (MIT OR Apache-2.0, <https://github.com/dioxuslabs/blitz>), restructured
//! onto a separate box tree: the DOM stays free of taffy/parley state and
//! anonymous boxes live only here. Everything the compute phase needs is
//! captured at box-tree construction time, so the taffy/parley passes never
//! touch the DOM or stylo styles.

pub mod construct;
pub mod engine;
pub mod fonts;
pub mod geometry;
pub mod images;
mod inline;
mod intrinsic_size;
mod marker;
pub mod multicol;
mod overflow;
pub mod pagination;
mod positioning;
mod replaced;
pub mod scroll;
pub mod scroll_into_view;
pub mod table;
mod taffy_impl;
pub mod text;
pub mod transform;
pub mod tree;
pub mod webfont;

pub use engine::{LayoutEngine, PaintStamp};
pub use fonts::{
    FontSystem, ParleyFontMetricsProvider, WebFontAttrs, WebFontOutcome, disable_system_fonts,
};
pub use geometry::{ClientBox, OffsetBox, ScrollParent, UsedBoxValues};
pub use images::{DecodedImage, ImageData, ImageId, ImageStore};
pub use multicol::{ColumnRange, MulticolContext};
pub use scroll::ScrollResult;
pub use scroll_into_view::{Align, scroll_into_view};
pub use tree::{
    BoxId, BoxKind, IfcData, LayoutBox, LayoutTree, PseudoBox, ReplacedContent, ReplacedContext,
    TextBrush,
};
