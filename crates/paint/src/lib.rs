//! Layout tree to display list (design doc §5.8, Phase 6, ADR-0007).
//!
//! The [`DisplayList`] is a flat, backend-neutral list of paint commands built
//! by walking the layout box tree in stacking-context order. It is immutable
//! and [`Send`] once built and carries an `Arc`-backed [`ResourceTable`], so
//! rasterization (`oxidepage-raster-skia`) and PDF export (`oxidepage-export-pdf`)
//! can run on any thread from the same list.

pub(crate) mod background;
pub(crate) mod builder;
pub(crate) mod convert;
pub mod decode;
pub mod display_list;
pub mod glyphs;
pub(crate) mod json;
pub mod path;
pub(crate) mod text;

pub use builder::{build_display_list, build_display_list_full};
pub use decode::{DecodedImageData, DecodedPixels, VectorImage, decode_image, rasterize_svg};
pub use display_list::{
    BorderEdge, BorderRadii, BorderStyle, Brush, Color, DecodedImage, DisplayItem, DisplayList,
    ExtendMode, FontId, FontResource, GradientStop, ImageData, ImageId, KAPPA, LinearGradient,
    PositionedGlyph, RadialGradient, ResourceTable, TileMode, border_edge_quads,
    uniform_border_geometry,
};
pub use glyphs::{PathCommand, glyph_index, glyph_outline};
pub use path::{PathSink, emit_path_commands, emit_rounded_rect};
