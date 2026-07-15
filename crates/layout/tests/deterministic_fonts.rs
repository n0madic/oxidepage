//! `disable_system_fonts()` must make a default-feature build shape text exactly
//! as a `--no-default-features` one does.
//!
//! Cargo unifies features across a build, so `layout/system_fonts` is compiled in
//! whenever any workspace member enables it — which the default does. The golden
//! and reftest runners rely on this runtime switch instead, and a golden compared
//! byte-for-byte across the 3-OS CI matrix silently drifts without it.
//!
//! This lives in its own test binary because the switch is process-wide.

use oxidepage_dom::{ParseOptions, parse_document};
use oxidepage_layout::{LayoutEngine, disable_system_fonts};
use oxidepage_style::{StyleEngine, Viewport};

/// The advance width of `text` rendered in `family` at 100px.
fn width_of(family: &str, text: &str) -> f32 {
    let html = format!(
        "<body style='margin:0'><span id=t style='font:100px {family}; display:inline-block'>{text}</span></body>"
    );
    let mut dom = parse_document(&html, ParseOptions::default()).tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut layout = LayoutEngine::new(Viewport::default());
    layout.reflow(&mut dom, &mut style);

    let span = dom
        .inclusive_descendants(dom.document())
        .find(|&id| {
            dom.node(id)
                .as_element()
                .is_some_and(|el| el.id().map(|a| &**a) == Some("t"))
        })
        .expect("span present");
    layout.border_box(span).expect("span has a box").size.width
}

#[test]
fn generic_families_fall_back_to_ahem_once_system_fonts_are_disabled() {
    disable_system_fonts();

    // Ahem's glyphs are full-em squares, so three of them at 100px measure 300px
    // in *every* generic family — the property goldens depend on. With a platform
    // font resolving `sans-serif`, the advances would differ per OS.
    let ahem = width_of("Ahem", "AAA");
    assert!((ahem - 300.0).abs() < 0.01, "Ahem baseline: {ahem}");

    for generic in ["sans-serif", "serif", "monospace", "cursive", "fantasy"] {
        let width = width_of(generic, "AAA");
        assert!(
            (width - ahem).abs() < 0.01,
            "`{generic}` must resolve to Ahem (got {width}, expected {ahem})"
        );
    }
}
