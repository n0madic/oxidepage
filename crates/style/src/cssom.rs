//! CSSOM operations on stylo declaration blocks (design doc §10, ADR-0005).
//!
//! The bindings crate drives `el.style` / `getComputedStyle` / rule declarations
//! through these plain-Rust helpers, so it never has to name stylo's property
//! internals directly (ADR-0005, decision 5). All value parsing is author-origin
//! `NoQuirks` — the CSSOM setters the WPT `css/cssom` suite exercises.

use cssparser::{Parser, ParserInput, ToCss as _};
use servo_arc::Arc as ServoArc;
use style::context::QuirksMode;
use style::media_queries::MediaList;
use style::parser::ParserContext;
use style::properties::{
    Importance, PropertyDeclarationBlock, PropertyId, SourcePropertyDeclaration,
    parse_one_declaration_into, parse_style_attribute,
};
use style::selector_parser::{PseudoElement, SelectorParser};
use style::shared_lock::{Locked, SharedRwLock, ToCssWithGuard};
use style::stylesheets::{
    CssRule, CssRuleType, DocumentStyleSheet, Origin, StylesheetInDocument, UrlExtraData,
};
use style_traits::ParsingMode;

/// Parses an HTML `media=""` attribute (or `@media`/`@import` condition) into a
/// stylo [`MediaList`]. An empty/whitespace query yields the always-matching
/// empty list. This is the sheet-level media list — passing it to
/// `Stylesheet::from_str`/`from_bytes` keeps the sheet's real rule structure
/// (including nested `@import`), unlike textually wrapping the CSS in `@media`.
#[must_use]
pub fn parse_media_list(media: &str, url_data: &UrlExtraData) -> MediaList {
    if media.trim().is_empty() {
        return MediaList::empty();
    }
    let context = ParserContext::new(
        Origin::Author,
        url_data,
        Some(CssRuleType::Media),
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        Default::default(),
        None,
        None,
        Default::default(),
    );
    let mut input = ParserInput::new(media);
    MediaList::parse(&context, &mut Parser::new(&mut input))
}

/// A shared reference to a locked declaration block (a style rule's `block`).
pub type LockedBlock = ServoArc<Locked<PropertyDeclarationBlock>>;

/// Parses an inline `style` attribute value into a declaration block.
#[must_use]
pub fn parse_inline_block(css: &str, url_data: &UrlExtraData) -> PropertyDeclarationBlock {
    parse_style_attribute(
        css,
        url_data,
        None,
        QuirksMode::NoQuirks,
        CssRuleType::Style,
    )
}

/// Serializes a declaration block to its `cssText`.
#[must_use]
pub fn block_to_css(block: &PropertyDeclarationBlock) -> String {
    let mut out = String::new();
    let _ = block.to_css(&mut out);
    out
}

/// `getPropertyValue`: the serialized value of `name`, or `""` when the property
/// is unknown or not set.
#[must_use]
pub fn block_get(block: &PropertyDeclarationBlock, name: &str) -> String {
    let Ok(id) = PropertyId::parse_enabled_for_all_content(name) else {
        return String::new();
    };
    let mut out = String::new();
    let _ = block.property_value_to_css(&id, &mut out);
    out
}

/// `getPropertyPriority`: whether `name` is declared `!important`.
#[must_use]
pub fn block_is_important(block: &PropertyDeclarationBlock, name: &str) -> bool {
    let Ok(id) = PropertyId::parse_enabled_for_all_content(name) else {
        return false;
    };
    block.property_priority(&id).important()
}

/// The property names declared in `block`, in declaration order (the indexed
/// properties a `CSSStyleDeclaration` exposes via `item`/`length`).
#[must_use]
pub fn block_names(block: &PropertyDeclarationBlock) -> Vec<String> {
    block
        .declarations()
        .iter()
        .map(|d| d.id().name().into_owned())
        .collect()
}

/// `setProperty`: parses `value` for `name` and sets it with the given priority.
/// Returns whether the block changed. Unknown properties and unparsable values
/// are ignored (CSSOM setProperty steps 5–6).
pub fn block_set(
    block: &mut PropertyDeclarationBlock,
    name: &str,
    value: &str,
    important: bool,
    url_data: &UrlExtraData,
) -> bool {
    let Ok(id) = PropertyId::parse_enabled_for_all_content(name) else {
        return false;
    };
    let mut decls = SourcePropertyDeclaration::default();
    if parse_one_declaration_into(
        &mut decls,
        id,
        value,
        Origin::Author,
        url_data,
        None,
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        CssRuleType::Style,
    )
    .is_err()
    {
        return false;
    }
    let importance = if important {
        Importance::Important
    } else {
        Importance::Normal
    };
    block.extend(decls.drain(), importance)
}

/// `removeProperty`: removes `name` from `block`, returning its previous
/// serialized value (`""` if it was absent or unknown).
pub fn block_remove(block: &mut PropertyDeclarationBlock, name: &str) -> String {
    let old = block_get(block, name);
    if let Ok(id) = PropertyId::parse_enabled_for_all_content(name)
        && let Some(first) = block.first_declaration_to_remove(&id)
    {
        block.remove_property(&id, first);
    }
    old
}

/// A `getComputedStyle` pseudo-element argument that names a pseudo OxidePage
/// does not support (v1: only `::before`/`::after`).
#[derive(Debug)]
pub struct UnsupportedPseudo;

/// Resolves a `getComputedStyle` pseudo-element argument to a stylo
/// [`PseudoElement`]. `None`/empty means the element itself; only `::before`
/// and `::after` are supported (ADR-0005 v1 scope). Returns
/// [`UnsupportedPseudo`] for a syntactically present but unsupported pseudo (the
/// caller returns `null`).
pub fn parse_pseudo(pseudo: Option<&str>) -> Result<Option<PseudoElement>, UnsupportedPseudo> {
    let Some(raw) = pseudo else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let name = raw
        .strip_prefix("::")
        .or_else(|| raw.strip_prefix(':'))
        .unwrap_or(raw);
    match name.to_ascii_lowercase().as_str() {
        "before" => Ok(Some(PseudoElement::Before)),
        "after" => Ok(Some(PseudoElement::After)),
        _ => Err(UnsupportedPseudo),
    }
}

// === Style sheets and rules (design doc §10, ADR-0005, WP-I) ===

/// The CSSOM `CSSRule.type` constant for `rule` (0 for rule types the CSSOM
/// assigns no legacy constant, per the spec note "return 0 from the API").
#[must_use]
pub fn rule_type_number(rule: &CssRule) -> u16 {
    let n = rule.rule_type() as u16;
    if (1..=14).contains(&n) { n } else { 0 }
}

/// Serializes a rule to its `cssText`.
#[must_use]
pub fn rule_css_text(lock: &SharedRwLock, rule: &CssRule) -> String {
    let guard = lock.read();
    let mut out = String::new();
    let _ = rule.to_css(&guard, &mut out);
    out
}

/// The top-level rules of a stylesheet, cloned (cheap: each is an `Arc`).
#[must_use]
pub fn sheet_rules(lock: &SharedRwLock, sheet: &DocumentStyleSheet) -> Vec<CssRule> {
    let guard = lock.read();
    sheet.contents(&guard).rules.read_with(&guard).0.clone()
}

/// A style rule's serialized selector list (`CSSStyleRule.selectorText`).
#[must_use]
pub fn style_rule_selector_text(lock: &SharedRwLock, rule: &CssRule) -> Option<String> {
    let guard = lock.read();
    match rule {
        CssRule::Style(s) => Some(s.read_with(&guard).selectors.to_css_string()),
        _ => None,
    }
}

/// Reparses and replaces a style rule's selector list. Returns whether it
/// changed (invalid selectors are ignored, leaving the rule untouched).
pub fn set_style_rule_selector_text(
    lock: &SharedRwLock,
    rule: &CssRule,
    text: &str,
    url_data: &UrlExtraData,
) -> bool {
    let CssRule::Style(s) = rule else {
        return false;
    };
    let Ok(list) = SelectorParser::parse_author_origin_no_namespace(text, url_data) else {
        return false;
    };
    let mut write = lock.write();
    s.write_with(&mut write).selectors = list;
    true
}

/// A style rule's locked declaration block (`CSSStyleRule.style`).
#[must_use]
pub fn style_rule_block(lock: &SharedRwLock, rule: &CssRule) -> Option<LockedBlock> {
    let guard = lock.read();
    match rule {
        CssRule::Style(s) => Some(s.read_with(&guard).block.clone()),
        _ => None,
    }
}

/// `getPropertyValue` against a rule's locked declaration block.
#[must_use]
pub fn locked_block_get(lock: &SharedRwLock, block: &LockedBlock, name: &str) -> String {
    block_get(block.read_with(&lock.read()), name)
}

/// `getPropertyPriority` against a rule's locked declaration block.
#[must_use]
pub fn locked_block_is_important(lock: &SharedRwLock, block: &LockedBlock, name: &str) -> bool {
    block_is_important(block.read_with(&lock.read()), name)
}

/// The declared property names of a rule's locked declaration block.
#[must_use]
pub fn locked_block_names(lock: &SharedRwLock, block: &LockedBlock) -> Vec<String> {
    block_names(block.read_with(&lock.read()))
}

/// `cssText` of a rule's locked declaration block.
#[must_use]
pub fn locked_block_to_css(lock: &SharedRwLock, block: &LockedBlock) -> String {
    block_to_css(block.read_with(&lock.read()))
}

/// `setProperty` against a rule's locked declaration block.
pub fn locked_block_set(
    lock: &SharedRwLock,
    block: &LockedBlock,
    name: &str,
    value: &str,
    important: bool,
    url_data: &UrlExtraData,
) -> bool {
    let mut write = lock.write();
    block_set(
        block.write_with(&mut write),
        name,
        value,
        important,
        url_data,
    )
}

/// `removeProperty` against a rule's locked declaration block.
pub fn locked_block_remove(lock: &SharedRwLock, block: &LockedBlock, name: &str) -> String {
    let mut write = lock.write();
    block_remove(block.write_with(&mut write), name)
}

/// Replaces the entire text of a rule's locked declaration block (`cssText`
/// setter): parses `css` as a declaration block and swaps it in.
pub fn locked_block_set_text(
    lock: &SharedRwLock,
    block: &LockedBlock,
    css: &str,
    url_data: &UrlExtraData,
) {
    let parsed = parse_inline_block(css, url_data);
    let mut write = lock.write();
    *block.write_with(&mut write) = parsed;
}
