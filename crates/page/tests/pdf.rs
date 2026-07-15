//! WP-M: `Page::print_to_pdf` smoke test — a real page produces a
//! structurally valid single-page PDF.

use oxidepage_page::{PageOptions, Viewport, load_html_page};

const VIEWPORT: Viewport = Viewport {
    width: 800.0,
    height: 600.0,
    dpr: 1.0,
};

#[test]
fn print_to_pdf_produces_a_valid_pdf() {
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0'>\
           <div style='width:200px;height:100px;background:#ff0000'></div>\
           <div style='font:16px Ahem'>XX</div>\
         </body>",
        PageOptions::default(),
    )
    .unwrap();
    let pdf = page.print_to_pdf();
    assert_eq!(&pdf[0..5], b"%PDF-", "PDF header");
    let text = String::from_utf8_lossy(&pdf);
    assert!(text.contains("/Catalog"));
    assert!(text.contains("/MediaBox"));
    assert!(text.contains("%%EOF"));
    // Phase 7: Ahem (TrueType) text is embedded as a selectable subset font.
    assert!(text.contains("/Font"), "font dictionary present");
    assert!(text.contains("/Type0"), "composite font");
    assert!(text.contains("/FontFile2"), "embedded subset font program");
    assert!(text.contains("/ToUnicode"), "ToUnicode CMap");
}

/// Regression: the PDF is a full-page render from the document top, so a
/// scripted viewport scroll must not shift or clip it (ADR-0007 D8).
#[test]
fn print_to_pdf_ignores_viewport_scroll() {
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0'>\
           <div style='width:100px;height:2000px;background:#00ff00'></div>\
         </body>",
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap();
    let pdf_top = page.print_to_pdf();

    // Scroll down (content is 2000px in an 800×600 viewport, so it scrolls).
    page.eval_to_string("window.scrollTo(0, 500)").unwrap();
    assert_eq!(
        page.eval_to_string("window.scrollY").unwrap(),
        "500",
        "the page actually scrolled"
    );

    let pdf_scrolled = page.print_to_pdf();
    assert_eq!(
        pdf_top, pdf_scrolled,
        "PDF renders the whole document from the top regardless of scroll"
    );
}
