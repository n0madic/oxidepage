//! Custom-element state and reaction intents (design doc; ADR custom-elements).
//!
//! The DOM crate cannot depend on JS, so custom-element *definitions*
//! (constructors, lifecycle callbacks) live in the bindings layer. The DOM
//! stores only per-element [`CustomElementState`] and a FIFO queue of
//! [`CustomElementReaction`] intents — pure data, no `JsValue`. Bindings tell
//! the DOM which names are defined (via [`DomTree::define_custom_element`]);
//! the DOM decides which elements get reaction intents; bindings match an
//! intent back to a constructor/callback when draining the queue.

use oxidepage_base::NodeId;

/// Custom-element lifecycle state, tracked per element
/// (HTML "custom element state").
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CustomElementState {
    /// A valid custom-element name that has not been upgraded yet.
    Undefined,
    /// A normal element that can never have reactions (not a valid custom
    /// element name, or not an HTML element). The default.
    #[default]
    Uncustomized,
    /// The element has been successfully upgraded (its constructor ran).
    Custom,
    /// The element's constructor threw during upgrade.
    Failed,
}

/// A queued custom-element reaction intent. Holds a `NodeId` *snapshot*; the
/// consumer must revalidate liveness (via [`DomTree::get`](crate::DomTree::get))
/// before acting on it, exactly as with the other DOM intent queues (L3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CustomElementReaction {
    /// Run the (already-defined) constructor against this element.
    Upgrade(NodeId),
    /// Deliver `connectedCallback`.
    Connected(NodeId),
    /// Deliver `disconnectedCallback`.
    Disconnected(NodeId),
    /// Deliver `attributeChangedCallback` (filtered by `observedAttributes`
    /// on the bindings side when the queue drains).
    AttributeChanged {
        node: NodeId,
        name: String,
        namespace: Option<String>,
        old: Option<String>,
        new: Option<String>,
    },
}

/// Names that match the PotentialCustomElementName grammar but are reserved by
/// other specifications (SVG/MathML) and are therefore *not* valid custom
/// element names.
const RESERVED_NAMES: &[&str] = &[
    "annotation-xml",
    "color-profile",
    "font-face",
    "font-face-src",
    "font-face-uri",
    "font-face-format",
    "font-face-name",
    "missing-glyph",
];

/// Whether `local` is a valid custom element name per the HTML "valid custom
/// element name" / PotentialCustomElementName production:
/// starts with `[a-z]`, contains a `-`, no ASCII uppercase, only allowed
/// PCENChar code points, and is not reserved.
#[must_use]
pub fn is_valid_custom_element_name(local: &str) -> bool {
    let mut chars = local.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    let mut has_hyphen = false;
    for c in chars {
        if c == '-' {
            has_hyphen = true;
        }
        if !is_pcen_char(c) {
            return false;
        }
        if c.is_ascii_uppercase() {
            return false;
        }
    }
    if !has_hyphen {
        return false;
    }
    !RESERVED_NAMES.contains(&local)
}

/// A PCENChar (potential-custom-element-name character), per HTML.
fn is_pcen_char(c: char) -> bool {
    matches!(c,
        '-' | '.' | '_' |
        '0'..='9' |
        'a'..='z' |
        '\u{00B7}' |
        '\u{00C0}'..='\u{00D6}' |
        '\u{00D8}'..='\u{00F6}' |
        '\u{00F8}'..='\u{037D}' |
        '\u{037F}'..='\u{1FFF}' |
        '\u{200C}'..='\u{200D}' |
        '\u{203F}'..='\u{2040}' |
        '\u{2070}'..='\u{218F}' |
        '\u{2C00}'..='\u{2FEF}' |
        '\u{3001}'..='\u{D7FF}' |
        '\u{F900}'..='\u{FDCF}' |
        '\u{FDF0}'..='\u{FFFD}' |
        '\u{10000}'..='\u{EFFFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_typical_custom_names() {
        assert!(is_valid_custom_element_name("x-foo"));
        assert!(is_valid_custom_element_name("swiper-container"));
        assert!(is_valid_custom_element_name("my-element-2"));
        assert!(is_valid_custom_element_name("a-"));
        assert!(is_valid_custom_element_name("font-face-x"));
    }

    #[test]
    fn rejects_names_without_hyphen() {
        assert!(!is_valid_custom_element_name("div"));
        assert!(!is_valid_custom_element_name("foo"));
        assert!(!is_valid_custom_element_name("x"));
    }

    #[test]
    fn rejects_bad_first_char() {
        assert!(!is_valid_custom_element_name("-x-foo"));
        assert!(!is_valid_custom_element_name("2x-foo"));
        assert!(!is_valid_custom_element_name("X-foo"));
    }

    #[test]
    fn rejects_uppercase() {
        assert!(!is_valid_custom_element_name("x-Foo"));
        assert!(!is_valid_custom_element_name("x-FOO"));
    }

    #[test]
    fn rejects_reserved() {
        assert!(!is_valid_custom_element_name("annotation-xml"));
        assert!(!is_valid_custom_element_name("color-profile"));
        assert!(!is_valid_custom_element_name("font-face"));
        assert!(!is_valid_custom_element_name("missing-glyph"));
    }
}
