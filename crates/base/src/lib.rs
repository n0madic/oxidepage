//! Shared primitives for the OxidePage engine.
//!
//! Everything here is dependency-light and usable from any other crate:
//! generation-checked ids, 2D geometry, the engine error hierarchy, and
//! re-exports of the interned string atoms shared with html5ever/stylo.

pub mod atoms;
pub mod error;
pub mod geometry;
pub mod id;

pub use error::{DomException, DomExceptionKind, EngineError, NetErrorKind};
pub use geometry::{Point, Rect, Size, Transform2D};
pub use id::{NodeId, RequestId, StyleSheetId};
