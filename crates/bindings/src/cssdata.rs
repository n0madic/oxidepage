//! Host data for the CSSOM interfaces.
//!
//! These are *views*: they store what identifies the source (an element, or the
//! `<style>`/`<link>` node that owns a sheet) and read the live data on every
//! access — the "correct by construction" model the DOM collections use (see
//! `collections.rs`). In particular a sheet/rule view holds the **owner node**,
//! not a snapshot `Arc`, so it follows the sheet across a re-parse of the
//! `<style>` (which replaces the underlying stylesheet).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use oxidepage_base::NodeId;
use oxidepage_dom::node::attr_name;
use oxidepage_dom::{LocalName, QualName};
use oxidepage_js::JsValue;
use servo_arc::Arc as ServoArc;
use style::properties::{ComputedValues, PropertyDeclarationBlock};
use style::selector_parser::PseudoElement;
use style::stylesheets::CssRule;

/// The `style` attribute's qualified name (shared by `el.style` reads/writes).
pub(crate) fn style_attr_name() -> QualName {
    attr_name(LocalName::from("style"))
}

/// Cache of an inline declaration block keyed by the `style` attribute text; a
/// read burst (`length` + `item(0..n)` + `getPropertyValue`) reparses once.
pub(crate) type InlineCache = RefCell<Option<(String, Rc<PropertyDeclarationBlock>)>>;

/// Cache of resolved computed values, valid while the engine and DOM versions
/// (author-style + DOM-mutation counters) are unchanged.
pub(crate) type ComputedCache = RefCell<Option<(u64, u64, ServoArc<ComputedValues>)>>;

/// What a `CSSStyleDeclaration` host object reflects.
pub(crate) enum StyleDeclData {
    /// `element.style`: the source of truth is the element's `style` attribute,
    /// so writes go back through the normal attribute-mutation path (snapshot +
    /// restyle).
    Inline { element: NodeId, block: InlineCache },
    /// `getComputedStyle(element, pseudo)`: read-only resolved cascade values.
    Computed {
        element: NodeId,
        pseudo: Option<PseudoElement>,
        cache: ComputedCache,
    },
    /// `CSSStyleRule.style`: the rule's declaration block, resolved live from
    /// `rule` on each access. Writes mutate it under the shared lock and notify
    /// the stylist. `owner` is the sheet's `<style>`/`<link>` node (for
    /// `parentRule` → same-identity sheet).
    Rule { owner: NodeId, rule: CssRule },
}

/// A `CSSStyleSheet`/`StyleSheet` host object.
pub(crate) enum SheetData {
    /// Owned by a `<style>`/`<link>` node; the current stylesheet is resolved
    /// from the engine on each access (follows re-parses).
    Node { owner: NodeId },
    /// `new CSSStyleSheet()` (constructable): the wrapper owns the sheet
    /// directly. `replaceSync` swaps its rules in place under the shared
    /// lock, so every adopting scope sees the update.
    Constructed {
        sheet: style::stylesheets::DocumentStyleSheet,
    },
}

impl SheetData {
    /// The owning `<style>`/`<link>` node, when there is one.
    pub(crate) fn owner(&self) -> Option<NodeId> {
        match self {
            SheetData::Node { owner } => Some(*owner),
            SheetData::Constructed { .. } => None,
        }
    }

    /// The constructed sheet, when this wrapper was made by `new CSSStyleSheet()`.
    pub(crate) fn constructed_sheet(&self) -> Option<style::stylesheets::DocumentStyleSheet> {
        match self {
            SheetData::Node { .. } => None,
            SheetData::Constructed { sheet } => Some(sheet.clone()),
        }
    }
}

/// A `CSSRule`/`CSSStyleRule` host object: a specific rule plus its owner node
/// (to resolve the current sheet for mutations and `parentStyleSheet`).
pub(crate) struct RuleData {
    pub owner: NodeId,
    pub rule: CssRule,
    /// `[SameObject]` cache for `CSSStyleRule.style`.
    pub style: RefCell<Option<JsValue>>,
}

/// A `CSSRuleList` host object over a sheet's top-level rules, with `[SameObject]`
/// per-index wrapper identity (`cssRules[0] === cssRules[0]`).
pub(crate) struct RuleListData {
    pub owner: NodeId,
    pub items: RefCell<HashMap<u32, JsValue>>,
}
