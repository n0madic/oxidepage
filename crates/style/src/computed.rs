//! Computed-value access for `getComputedStyle` (design doc §10, ADR-0005).
//!
//! Phase 4 returns *computed* values (used/resolved values arrive with layout in
//! Phase 5). Shorthands serialize to `""` in the computed declaration (a v1
//! limitation; WPT `css/cssom` is overwhelmingly longhands).

use oxidepage_dom::select::{NodeRef, enter_active_tree};
use oxidepage_dom::{DomTree, NodeId};
use servo_arc::Arc as ServoArc;
use style::dom::TElement;
use style::properties::{ComputedValues, PropertyDeclarationId, PropertyId};
use style::selector_parser::PseudoElement;

use crate::engine::StyleEngine;

/// Serializes the computed value of the CSS property `name` on `cv`.
///
/// Longhands and custom properties serialize normally; shorthands and unknown
/// properties yield `""`.
#[must_use]
pub fn serialize_property(cv: &ComputedValues, name: &str) -> String {
    let Ok(property_id) = PropertyId::parse_enabled_for_all_content(name) else {
        return String::new();
    };
    match property_id.as_shorthand() {
        // Shorthands are not serialized in the computed declaration (v1).
        Ok(_shorthand) => String::new(),
        Err(PropertyDeclarationId::Longhand(id)) => {
            let physical = id.to_physical(cv.writing_mode);
            cv.computed_value_to_string(PropertyDeclarationId::Longhand(physical))
        }
        Err(custom @ PropertyDeclarationId::Custom(_)) => cv.computed_value_to_string(custom),
    }
}

/// The computed style for `node` (or its `pseudo` pseudo-element), resolving the
/// document first so the values are up to date.
///
/// Returns `None` if the element has no cascade data (e.g. it lives in a
/// `display: none` subtree that stylo never styled, or the requested pseudo does
/// not exist).
#[must_use]
pub fn computed_style_for(
    engine: &mut StyleEngine,
    tree: &mut DomTree,
    node: NodeId,
    pseudo: Option<PseudoElement>,
) -> Option<ServoArc<ComputedValues>> {
    engine.resolve_styles(tree);
    let scope = enter_active_tree(tree);
    let el = NodeRef::new(&scope, node);
    let data = el.borrow_data()?;
    match pseudo {
        None => data.styles.get_primary().cloned(),
        Some(pe) => data.styles.pseudos.get(&pe).cloned(),
    }
}
