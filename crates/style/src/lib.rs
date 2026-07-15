//! Stylo integration: the document stylesheet set, media queries, incremental
//! restyle, computed-value access, and the CSSOM-facing engine operations
//! (design doc §10, ADR-0005).

pub mod computed;
pub mod cssom;
pub mod engine;
pub mod font_faces;
pub mod fonts;
pub mod loader;
pub mod properties;

pub use computed::{computed_style_for, serialize_property};
pub use engine::{StyleEngine, Viewport};
pub use font_faces::{FontFaceInfo, FontFaceSource, FontFaceStyle, FontFormatHint};
pub use loader::{BlockingImportLoader, CssFetcher};
pub use properties::{
    css_to_idl_attribute, longhand_names_sorted, supported_property_names, webkit_idl_attribute,
};
