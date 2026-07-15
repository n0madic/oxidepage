//! Viewport vs. full-page screenshot geometry (ADR-0007 D8).

use oxidepage_page::{PageOptions, RasterOptions, Viewport, load_html_page};

const VIEWPORT: Viewport = Viewport {
    width: 800.0,
    height: 600.0,
    dpr: 1.0,
};

/// A green strip at the document top, then 2000px of red — taller than the
/// 600px viewport, so the bottom of the document is off-screen.
fn tall_page() -> oxidepage_page::Page {
    load_html_page(
        "<!DOCTYPE html><body style='margin:0'>\
           <div style='width:800px;height:100px;background:#00ff00'></div>\
           <div style='width:800px;height:2000px;background:#ff0000'></div>\
         </body>",
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn viewport_screenshot_is_viewport_sized() {
    let image = tall_page().render_pixels(&RasterOptions::default());
    assert_eq!((image.width, image.height), (800, 600));
}

#[test]
fn full_page_screenshot_covers_the_whole_document() {
    let image = tall_page().render_pixels_full_page(&RasterOptions::default());
    assert_eq!((image.width, image.height), (800, 2100));
    // Green strip at the top, red all the way down to the last row.
    assert_eq!(image.pixel(400, 50), [0, 255, 0, 255]);
    assert_eq!(image.pixel(400, 2099), [255, 0, 0, 255]);
}

#[test]
fn full_page_screenshot_honors_dpr() {
    let image = tall_page().render_pixels_full_page(&RasterOptions {
        scale: 2.0,
        ..RasterOptions::default()
    });
    assert_eq!((image.width, image.height), (1600, 4200));
}

/// Regression: the full-page render starts at the document top, so a scripted
/// viewport scroll must not shift or clip it — unlike a viewport screenshot.
#[test]
fn full_page_screenshot_ignores_viewport_scroll() {
    let page = tall_page();
    let top = page.screenshot_full_page(1.0);
    let viewport_top = page.screenshot(1.0);

    page.eval_to_string("window.scrollTo(0, 500)").unwrap();
    assert_eq!(
        page.eval_to_string("window.scrollY").unwrap(),
        "500",
        "the page actually scrolled"
    );

    assert!(!top.is_empty(), "PNG encoded");
    assert_eq!(
        top,
        page.screenshot_full_page(1.0),
        "the full-page screenshot renders the whole document from the top"
    );
    assert_ne!(
        viewport_top,
        page.screenshot(1.0),
        "the viewport screenshot follows the scroll"
    );
}

/// A green strip (doc y 0–100) over red, plus a `position: fixed` blue box
/// pinned at the top-left. Scrolling the document must shift the document
/// content in a viewport render while leaving the fixed box exactly where it is
/// — the whole point of building the display list unscrolled and applying the
/// scroll at raster time.
#[test]
fn viewport_render_scrolls_document_but_pins_fixed() {
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0'>\
           <div style='position:fixed;top:0;left:0;width:50px;height:50px;background:#0000ff'></div>\
           <div style='width:800px;height:100px;background:#00ff00'></div>\
           <div style='width:800px;height:2000px;background:#ff0000'></div>\
         </body>",
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap();

    let top = page.render_pixels(&RasterOptions::default());
    // Under the fixed box: blue. Beside it, in the document: green (doc y 25).
    assert_eq!(top.pixel(25, 25), [0, 0, 255, 255], "fixed box paints blue");
    assert_eq!(
        top.pixel(100, 25),
        [0, 255, 0, 255],
        "document green strip beside the fixed box"
    );

    page.eval_to_string("window.scrollTo(0, 400)").unwrap();
    let scrolled = page.render_pixels(&RasterOptions::default());

    // The fixed box is unmoved (still blue at the same pixel)…
    assert_eq!(
        scrolled.pixel(25, 25),
        [0, 0, 255, 255],
        "the fixed box stays pinned to the viewport across a document scroll"
    );
    // …while the document scrolled: doc y 425 is now red where green used to be.
    assert_eq!(
        scrolled.pixel(100, 25),
        [255, 0, 0, 255],
        "document content shifted up by the scroll offset"
    );
}
