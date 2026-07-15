//! CSS property enumeration and the CSSOM property-name ↔ IDL-attribute
//! mapping (design doc §10, ADR-0005).
//!
//! `LonghandId`/`ShorthandId` are `#[repr(u16)]` with contiguous discriminants
//! `0..N`, so we enumerate them by transmuting the index — the same trick stylo
//! itself uses internally. Hence the scoped `unsafe` allow.
#![allow(unsafe_code)]

use std::sync::LazyLock;

use style::properties::{LonghandId, PropertyId, ShorthandId, property_counts};

/// Whether `name` names a property exposed to author content (enabled).
fn is_enabled(name: &str) -> bool {
    PropertyId::parse_enabled_for_all_content(name).is_ok()
}

/// The enabled longhand names, sorted — a compile-time constant computed once
/// (the indexed properties a computed `CSSStyleDeclaration` walks per read).
static LONGHAND_NAMES_SORTED: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    let mut names: Vec<&'static str> = (0..property_counts::LONGHANDS as u16)
        // SAFETY: `LonghandId` is `#[repr(u16)]` with discriminants `0..LONGHANDS`.
        .map(|i| unsafe { std::mem::transmute::<u16, LonghandId>(i) }.name())
        .filter(|name| is_enabled(name))
        .collect();
    names.sort_unstable();
    names
});

/// All enabled longhand property names, sorted — the indexed properties a
/// computed `CSSStyleDeclaration` exposes (`getComputedStyle`).
#[must_use]
pub fn longhand_names_sorted() -> &'static [&'static str] {
    &LONGHAND_NAMES_SORTED
}

/// All enabled longhand and shorthand property names — the set for which
/// `CSSStyleDeclaration` exposes camel-cased/dashed accessors.
#[must_use]
pub fn supported_property_names() -> Vec<&'static str> {
    let longhands = (0..property_counts::LONGHANDS as u16)
        // SAFETY: `LonghandId` is `#[repr(u16)]` with discriminants `0..LONGHANDS`.
        .map(|i| unsafe { std::mem::transmute::<u16, LonghandId>(i) }.name());
    let shorthands = (0..property_counts::SHORTHANDS as u16)
        // SAFETY: `ShorthandId` is `#[repr(u16)]` with discriminants `0..SHORTHANDS`.
        .map(|i| unsafe { std::mem::transmute::<u16, ShorthandId>(i) }.name());
    longhands
        .chain(shorthands)
        .filter(|name| is_enabled(name))
        .collect()
}

/// Converts a CSS property name to its CSSOM IDL attribute (camel case), per the
/// CSSOM "CSS property to IDL attribute" algorithm, with the `float` → `cssFloat`
/// special case.
#[must_use]
pub fn css_to_idl_attribute(name: &str) -> String {
    if name.eq_ignore_ascii_case("float") {
        return "cssFloat".to_owned();
    }
    let mut output = String::with_capacity(name.len());
    let mut uppercase_next = false;
    for c in name.chars() {
        if c == '-' {
            uppercase_next = true;
        } else if uppercase_next {
            output.push(c.to_ascii_uppercase());
            uppercase_next = false;
        } else {
            output.push(c);
        }
    }
    output
}

/// The additional lowercase-first *webkit-cased* IDL attribute the CSSOM defines
/// for `-webkit-`-prefixed properties (`-webkit-transform` → `webkitTransform`,
/// alongside the camel-cased `WebkitTransform`). `None` for other properties.
#[must_use]
pub fn webkit_idl_attribute(name: &str) -> Option<String> {
    if !name.starts_with("-webkit-") {
        return None;
    }
    let camel = css_to_idl_attribute(name);
    let mut chars = camel.chars();
    chars
        .next()
        .map(|first| format!("{}{}", first.to_ascii_lowercase(), chars.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idl_attribute_mapping() {
        assert_eq!(css_to_idl_attribute("background-color"), "backgroundColor");
        assert_eq!(css_to_idl_attribute("color"), "color");
        assert_eq!(css_to_idl_attribute("float"), "cssFloat");
        assert_eq!(css_to_idl_attribute("-webkit-transform"), "WebkitTransform");
    }

    #[test]
    fn property_tables_are_populated_and_enabled() {
        let longhands = longhand_names_sorted();
        assert!(longhands.contains(&"display"));
        assert!(longhands.contains(&"color"));
        // Sorted.
        assert!(longhands.windows(2).all(|w| w[0] <= w[1]));

        let all = supported_property_names();
        assert!(all.contains(&"display"));
        assert!(all.contains(&"margin"), "shorthands are included");
    }
}
