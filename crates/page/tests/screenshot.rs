//! Viewport vs. full-page screenshot geometry (ADR-0007 D8).

use oxidepage_base::Rect;
use oxidepage_page::{
    ImageFormat, PageOptions, RasterOptions, ScreenshotOptions, Viewport, load_html_page,
};

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

// === Clip and format (ADR-0026) ===

#[test]
fn a_clip_sizes_the_output_and_picks_the_document_region() {
    // A 400×300 window over the red band, 500px down the document.
    let image = tall_page().render_pixels_with(&ScreenshotOptions {
        clip: Some(Rect::from_xywh(0.0, 500.0, 400.0, 300.0)),
        ..ScreenshotOptions::default()
    });
    assert_eq!((image.width, image.height), (400, 300));
    assert_eq!(image.pixel(200, 150), [255, 0, 0, 255]);

    // A clip straddling the boundary sees both bands: the green strip runs to
    // y = 100, so a clip starting at y = 50 has green on top and red below.
    let image = tall_page().render_pixels_with(&ScreenshotOptions {
        clip: Some(Rect::from_xywh(0.0, 50.0, 200.0, 200.0)),
        ..ScreenshotOptions::default()
    });
    assert_eq!(image.pixel(100, 10), [0, 255, 0, 255], "still in the strip");
    assert_eq!(image.pixel(100, 100), [255, 0, 0, 255], "past it");
}

#[test]
fn a_clip_scales_with_dpr() {
    let image = tall_page().render_pixels_with(&ScreenshotOptions {
        dpr: 2.0,
        clip: Some(Rect::from_xywh(0.0, 500.0, 400.0, 300.0)),
        ..ScreenshotOptions::default()
    });
    assert_eq!((image.width, image.height), (800, 600));
    assert_eq!(image.pixel(400, 300), [255, 0, 0, 255]);
}

#[test]
fn a_clip_wins_over_full_page() {
    let image = tall_page().render_pixels_with(&ScreenshotOptions {
        full_page: true,
        clip: Some(Rect::from_xywh(0.0, 0.0, 100.0, 100.0)),
        ..ScreenshotOptions::default()
    });
    assert_eq!((image.width, image.height), (100, 100));
}

#[test]
fn jpeg_output_is_a_valid_jfif_stream() {
    let bytes = tall_page().screenshot_with(&ScreenshotOptions {
        format: ImageFormat::Jpeg,
        ..ScreenshotOptions::default()
    });
    assert!(!bytes.is_empty());
    // SOI marker, and EOI at the end.
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "JPEG SOI");
    assert_eq!(&bytes[bytes.len() - 2..], &[0xFF, 0xD9], "JPEG EOI");
    // Round-trips through a decoder at the requested size.
    let decoded = image::load_from_memory(&bytes).expect("decodes");
    assert_eq!((decoded.width(), decoded.height()), (800, 600));
}

#[test]
fn jpeg_quality_changes_the_size() {
    let page = tall_page();
    let low = page.screenshot_with(&ScreenshotOptions {
        format: ImageFormat::Jpeg,
        quality: 10,
        ..ScreenshotOptions::default()
    });
    let high = page.screenshot_with(&ScreenshotOptions {
        format: ImageFormat::Jpeg,
        quality: 95,
        ..ScreenshotOptions::default()
    });
    assert!(low.len() < high.len(), "{} vs {}", low.len(), high.len());
}

#[test]
fn png_is_still_the_default_format() {
    let bytes = tall_page().screenshot_with(&ScreenshotOptions::default());
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    // …and the wrapper is the same picture.
    assert_eq!(bytes, tall_page().screenshot(1.0));
}
