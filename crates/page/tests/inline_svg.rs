//! Inline `<svg>` as a vector replaced element (ADR-0013 D5).
//!
//! Two properties the old intrinsic-size rasterization could not have:
//!
//! * `currentColor` resolves. resvg renders the SVG in isolation and knows
//!   nothing of the surrounding cascade, so the page has to embed the element's
//!   computed `color` in the markup it stores. Without that, `fill="currentColor"`
//!   falls back to black.
//! * A `color` change re-rasterizes. The computed color is part of the store key,
//!   so recoloring an icon from script produces a new entry rather than reusing
//!   the old pixels.
//! * `fill="var(--x)"` resolves. resvg resolves no `var()` either, so the page
//!   substitutes the custom properties the cascade computed on the `<svg>` before
//!   storing the markup — otherwise a themed icon paints solid black.

use std::time::Duration;

use oxidepage_page::{PageOptions, RasterOptions, Viewport, load_html_page};

const VIEWPORT: Viewport = Viewport {
    width: 100.0,
    height: 100.0,
    dpr: 1.0,
};

/// A page whose only content is a 50×50 icon filled with `currentColor`, under
/// the CSS `color` given.
fn icon_page(color: &str) -> oxidepage_page::Page {
    load_html_page(
        &format!(
            "<!DOCTYPE html><body style='margin:0'>\
               <svg id='i' width='50' height='50' viewBox='0 0 10 10' style='color:{color}'>\
                 <rect width='10' height='10' fill='currentColor'/>\
               </svg>\
             </body>"
        ),
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn current_color_resolves_to_the_computed_css_color() {
    let image = icon_page("green").render_pixels(&RasterOptions::default());
    assert_eq!(
        image.pixel(25, 25),
        [0, 128, 0, 255],
        "fill=\"currentColor\" must resolve to the element's computed color, not black"
    );
}

#[test]
fn recoloring_from_script_re_rasterizes_the_icon() {
    let page = icon_page("green");
    assert_eq!(
        page.render_pixels(&RasterOptions::default()).pixel(25, 25),
        [0, 128, 0, 255]
    );

    page.eval_to_string("document.getElementById('i').style.color = 'red'")
        .unwrap();
    page.settle(Duration::from_secs(5));

    assert_eq!(
        page.render_pixels(&RasterOptions::default()).pixel(25, 25),
        [255, 0, 0, 255],
        "a color change must produce a new store key and a fresh rasterization, \
         not reuse the green pixels"
    );
}

#[test]
fn fill_var_resolves_to_a_custom_property() {
    // resvg resolves no `var()`, so an icon filled with a CSS custom property —
    // how Tailwind & co. theme SVGs — would fall back to black. The cascade
    // resolved `--bg` on the `<svg>`; the page substitutes it before handing the
    // markup to resvg.
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0'>\
           <svg id='i' width='50' height='50' viewBox='0 0 10 10' style='--bg:#0000ff'>\
             <rect width='10' height='10' fill='var(--bg)'/>\
           </svg>\
         </body>",
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        page.render_pixels(&RasterOptions::default()).pixel(25, 25),
        [0, 0, 255, 255],
        "fill=\"var(--bg)\" must resolve to the element's custom property, not black"
    );
}

/// A sprite icon: a hidden `<svg>` defines a `<symbol>`, and a separate
/// `<svg><use href="#id"></svg>` references it — the Bootstrap icon pattern.
/// resvg renders each `<svg>` in isolation and cannot reach the symbol in the
/// other tree, so without inlining the definition the icon decoded as broken and
/// painted a grey placeholder square. The `xlink:href` form (no `xmlns:xlink`
/// declaration on the isolated fragment) additionally made usvg reject the whole
/// document as malformed XML.
fn sprite_page(use_attr: &str) -> oxidepage_page::Page {
    load_html_page(
        &format!(
            "<!DOCTYPE html><body style='margin:0'>\
               <svg style='display:none'>\
                 <symbol id='dot' viewBox='0 0 10 10'>\
                   <rect width='10' height='10' fill='currentColor'/>\
                 </symbol>\
               </svg>\
               <svg id='i' width='50' height='50' style='color:green'>\
                 <use {use_attr}='#dot'></use>\
               </svg>\
             </body>"
        ),
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn sprite_use_xlink_href_renders_the_referenced_symbol() {
    // Bootstrap's exact form: `<use xlink:href="#id">`, no size or `viewBox` on
    // the icon `<svg>` beyond its CSS `width`/`height`.
    let image = sprite_page("xlink:href").render_pixels(&RasterOptions::default());
    let center = image.pixel(25, 25);
    assert_ne!(
        center,
        [192, 192, 192, 255],
        "the icon must render its symbol, not the broken-image grey square"
    );
    assert_eq!(
        center,
        [0, 128, 0, 255],
        "the inlined symbol's fill=\"currentColor\" must resolve to the icon's color"
    );
}

#[test]
fn sprite_use_plain_href_renders_the_referenced_symbol() {
    // The SVG2 `href` form (no xlink) must resolve identically.
    let image = sprite_page("href").render_pixels(&RasterOptions::default());
    assert_eq!(image.pixel(25, 25), [0, 128, 0, 255]);
}

#[test]
fn sprite_symbol_without_fill_still_renders_a_shape() {
    // Real icon sprites (Bootstrap Icons) leave the symbol paths unfilled and
    // colour them from the stylesheet (`.bi { fill: currentColor }`), which resvg
    // does not see. The engine models no SVG `fill` property, so such a symbol
    // paints with resvg's default black — but it must paint a *shape*, never the
    // broken-image grey placeholder the reference bug produced.
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0'>\
           <svg style='display:none'>\
             <symbol id='dot' viewBox='0 0 10 10'><rect width='10' height='10'/></symbol>\
           </svg>\
           <svg id='i' width='50' height='50'><use xlink:href='#dot'></use></svg>\
         </body>",
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap();
    let center = page.render_pixels(&RasterOptions::default()).pixel(25, 25);
    assert_ne!(
        center,
        [192, 192, 192, 255],
        "an unfilled sprite symbol must still render its shape, not the grey square"
    );
    assert_eq!(
        center,
        [0, 0, 0, 255],
        "the default SVG fill is opaque black"
    );
}

#[test]
fn the_icon_is_sharp_at_ten_times_its_view_box() {
    // The 10×10 viewBox is shown at 50×50 CSS px. Rasterized at the device size
    // the fill is a flat, exact color right up to the edge.
    let image = icon_page("green").render_pixels(&RasterOptions {
        scale: 2.0,
        ..RasterOptions::default()
    });
    assert_eq!((image.width, image.height), (200, 200));
    // Deep inside the icon (device px 0..100) and just inside its bottom-right
    // corner: both pure, no blur from an upscaled 10×10 bitmap.
    assert_eq!(image.pixel(50, 50), [0, 128, 0, 255]);
    assert_eq!(image.pixel(98, 98), [0, 128, 0, 255]);
}
