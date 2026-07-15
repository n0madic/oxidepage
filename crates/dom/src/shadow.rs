//! Shadow DOM model types (DOM spec §4.8).
//!
//! A shadow root is modeled like `<template>` contents: a `DocumentFragment`
//! node linked bidirectionally to its host (element → `ElementData::shadow_root`,
//! fragment → `NodeData::DocumentFragment { host, shadow: Some(mode) }`).
//! Unlike template contents, a shadow tree *participates* in connectedness,
//! style, layout (via the flat tree) and the composed event path.

/// Shadow root mode (DOM `ShadowRootMode`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShadowMode {
    Open,
    Closed,
}

impl ShadowMode {
    /// The IDL string form (`"open"` / `"closed"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ShadowMode::Open => "open",
            ShadowMode::Closed => "closed",
        }
    }
}

/// Whether `local` names an element that may host a shadow root, per the
/// `attachShadow` steps: a fixed list of HTML container elements, or any
/// valid custom element name.
#[must_use]
pub fn is_valid_shadow_host_name(local: &str) -> bool {
    matches!(
        local,
        "article"
            | "aside"
            | "blockquote"
            | "body"
            | "div"
            | "footer"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "main"
            | "nav"
            | "p"
            | "section"
            | "span"
    ) || crate::custom_element::is_valid_custom_element_name(local)
}
