//! WP-H: the parley/skrifa font-metrics provider resolves `ex`/`ch` units
//! from real font metrics. Ahem has x-height = 0.8em and '0' advance = 1em.

use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};
use oxidepage_layout::fonts::AHEM_FONT;
use oxidepage_layout::{LayoutEngine, WebFontAttrs, WebFontOutcome};
use oxidepage_style::{
    FontFaceInfo, FontFaceStyle, StyleEngine, Viewport, computed_style_for, serialize_property,
};

fn find_by_id(tree: &DomTree, id_attr: &str) -> NodeId {
    tree.inclusive_descendants(tree.document())
        .find(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| el.id().map(|a| &**a) == Some(id_attr))
        })
        .unwrap_or_else(|| panic!("no element with id={id_attr}"))
}

#[test]
fn ex_and_ch_units_resolve_from_ahem_metrics() {
    let mut dom = parse_document(
        "<div id=d style='font-family: Ahem; font-size: 20px; \
         width: 10ex; height: 10ch'></div>",
        ParseOptions::default(),
    )
    .tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let layout = LayoutEngine::new(Viewport::default());
    style.set_font_metrics_provider(layout.font_metrics_factory());

    let d = find_by_id(&dom, "d");
    let cv = computed_style_for(&mut style, &mut dom, d, None).expect("computed style");
    // Ahem: x-height = 0.8em → 10ex at 20px = 160px.
    assert_eq!(serialize_property(&cv, "width"), "160px");
    // Ahem: '0' advance = 1em → 10ch at 20px = 200px.
    assert_eq!(serialize_property(&cv, "height"), "200px");
}

/// Regression: registering a web font bumps the layout engine's fonts version,
/// which re-shapes text — but stylo caches `ComputedValues` per element and
/// reuses them until the cascade is dirtied. Without an explicit invalidation
/// the metric-dependent units (`ex`/`ch`/`ic`) keep the values they resolved to
/// before the face existed, forever.
#[test]
fn a_registered_web_font_recascades_metric_dependent_values() {
    // `WebAhem` exists in no collection when the first cascade runs.
    let mut dom = parse_document(
        "<div id=d style='font-family: WebAhem; font-size: 20px; width: 10ch'></div>",
        ParseOptions::default(),
    )
    .tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut layout = LayoutEngine::new(Viewport::default());
    style.set_font_metrics_provider(layout.font_metrics_factory());

    let d = find_by_id(&dom, "d");
    let cv = computed_style_for(&mut style, &mut dom, d, None).expect("computed style");
    // No face answers the metrics query, so `ch` falls back to 0.5em: 10 × 10px.
    assert_eq!(serialize_property(&cv, "width"), "100px");

    let attrs = WebFontAttrs::from_face(&FontFaceInfo {
        family: "WebAhem".to_owned(),
        sources: Vec::new(),
        unicode_range: None,
        weight: (400.0, 400.0),
        style: FontFaceStyle::Normal,
        stretch: 100.0,
    });
    assert_eq!(
        layout.register_web_font("WebAhem", AHEM_FONT, attrs),
        WebFontOutcome::Registered
    );
    style.note_fonts_changed();

    let cv = computed_style_for(&mut style, &mut dom, d, None).expect("computed style");
    // Ahem's '0' advance is a full em → 10ch at 20px = 200px.
    assert_eq!(serialize_property(&cv, "width"), "200px");
}

#[test]
fn without_provider_ex_falls_back_to_half_em() {
    // The default (noop) provider reports no metrics; stylo then falls back
    // to 0.5em per spec.
    let mut dom = parse_document(
        "<div id=d style='font-size: 20px; width: 10ex'></div>",
        ParseOptions::default(),
    )
    .tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let d = find_by_id(&dom, "d");
    let cv = computed_style_for(&mut style, &mut dom, d, None).expect("computed style");
    assert_eq!(serialize_property(&cv, "width"), "100px");
}
