//! `Page::print_to_pdf` / `Page::pdf`: a real page produces a structurally
//! valid PDF, paginated onto real paper, breaking between lines rather than
//! through them (WP-M, ADR-0026).

use oxidepage_page::{PageOptions, PaintOptions, PdfOptions, Viewport, load_html_page};

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

// === Pagination and print options (ADR-0026) ===

/// The `/Count` of the page tree.
fn page_count(pdf: &[u8]) -> usize {
    let s = String::from_utf8_lossy(pdf);
    let at = s.find("/Count ").expect("page tree count");
    s[at + 7..]
        .split(|c: char| !c.is_ascii_digit())
        .find(|t| !t.is_empty())
        .expect("a number")
        .parse()
        .expect("a number")
}

fn tall_page() -> oxidepage_page::Page {
    load_html_page(
        "<!DOCTYPE html><body style='margin:0'>\
           <div style='width:100px;height:5000px;background:#00ff00'></div>\
         </body>",
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn print_to_pdf_paginates_onto_a4_by_default() {
    let pdf = tall_page().print_to_pdf();
    let text = String::from_utf8_lossy(&pdf);
    // A4 in points: 793.7 × 1122.52 CSS px × 0.75.
    assert!(text.contains("/MediaBox [0 0 595.27"), "A4 media box");
    assert!(page_count(&pdf) > 1, "a 5000px document is several pages");
    assert_eq!(
        text.matches("/Subtype /Form").count(),
        1,
        "the document content is emitted once and shared by every page"
    );
}

#[test]
fn pagination_breaks_between_lines_not_through_them() {
    // 400 lines of 10px Ahem text on A4: every page boundary has to land on a
    // line top, which — with `line-height: 10px` and a `margin: 0` body — means
    // every boundary is a multiple of 10.
    let page = load_html_page(
        &format!(
            "<!DOCTYPE html><body style='margin:0'>\
             <div style='font:10px/10px Ahem'>{}</div></body>",
            "<p style='margin:0'>xxxx</p>".repeat(400)
        ),
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap();
    let boundaries = page.page_boundaries(1000.0);
    assert!(boundaries.len() > 3, "several pages: {boundaries:?}");
    for offset in &boundaries[1..boundaries.len() - 1] {
        assert_eq!(
            offset % 10.0,
            0.0,
            "boundary {offset} is not on a line top: {boundaries:?}"
        );
    }
    for pair in boundaries.windows(2) {
        assert!(
            pair[0] < pair[1],
            "boundaries must increase: {boundaries:?}"
        );
    }
    // Greedy: each page is filled as far as it can be, so no boundary is more
    // than one line short of the page height.
    for pair in boundaries.windows(2) {
        assert!(
            pair[1] - pair[0] <= 1000.0 + 0.01,
            "page {pair:?} overflows the 1000px slice"
        );
        if pair[1] < *boundaries.last().unwrap() {
            assert!(
                pair[1] - pair[0] > 1000.0 - 10.0 - 0.01,
                "page {pair:?} stops more than one line early"
            );
        }
    }
}

#[test]
fn a_block_with_no_break_opportunity_still_paginates() {
    // A single tall block offers no class-A break point, and a `display: flex`
    // body offers none either. Multicol lets such content overflow its column;
    // paper cannot, so the fill falls back to the page boundary (CSS
    // Fragmentation §3.4). Without it this document would print as one page as
    // tall as itself — the bug pagination exists to fix.
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0;display:flex'>\
           <div style='width:100px;height:5000px;background:#00ff00'></div>\
         </body>",
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap();
    let boundaries = page.page_boundaries(1000.0);
    assert_eq!(boundaries.len(), 6, "5000 / 1000: {boundaries:?}");
    assert_eq!(boundaries[1], 1000.0);
    assert_eq!(*boundaries.last().unwrap(), 5000.0);
}

#[test]
fn paginate_false_restores_the_single_tall_page() {
    let pdf = tall_page().pdf(
        &PdfOptions {
            paginate: false,
            ..PdfOptions::default()
        },
        &PaintOptions::default(),
    );
    assert_eq!(page_count(&pdf), 1);
    // 800 × 5000 CSS px × 0.75 = 600 × 3750 pt.
    assert!(String::from_utf8_lossy(&pdf).contains("/MediaBox [0 0 600 3750]"));
}

#[test]
fn print_background_false_drops_the_background_fill() {
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0;background:#123456'>\
           <div style='width:100px;height:50px;background:#ff0000'></div>\
         </body>",
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap();
    // Backgrounds on by default (this engine's divergence from Chrome), so the
    // red fill and the propagated canvas colour are both emitted.
    let with = String::from_utf8_lossy(&page.pdf(&PdfOptions::default(), &PaintOptions::default()))
        .into_owned();
    assert!(with.contains("1 0 0 rg"), "the red fill: {with}");
    assert!(
        with.contains("0.07058824 0.20392157 0.3372549 rg"),
        "the #123456 canvas: {with}"
    );

    let without = String::from_utf8_lossy(&page.pdf(
        &PdfOptions::default(),
        &PaintOptions {
            print_background: false,
        },
    ))
    .into_owned();
    assert!(!without.contains("1 0 0 rg"), "no red fill: {without}");
    assert!(
        !without.contains("0.07058824 0.20392157 0.3372549 rg"),
        "no canvas colour: {without}"
    );
    // The opaque white paper stays.
    assert!(without.contains("1 1 1 rg"), "white base: {without}");
}

#[test]
fn a_short_document_is_exactly_one_page() {
    // Regression: layout reported the bare content extent as the last boundary
    // while the exporter's document box is floored by the viewport, so the
    // extent became an *interior* break and every short page gained a blank
    // trailing sheet.
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0'><p>hello</p></body>",
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap();
    assert_eq!(page.page_boundaries(1166.0).len(), 2, "one page");
    assert_eq!(page_count(&page.print_to_pdf()), 1);
}

#[test]
fn each_page_clips_to_its_own_slice() {
    // The greedy fill stops at a line top above the page bottom, so the strip
    // between the break and the page's full content height belongs to the
    // *next* page. Clipping to the full content box drew it at the foot of this
    // one and again at the head of the next — the cut-line artefact pagination
    // exists to prevent. Each page's clip height must be its own slice.
    let page = load_html_page(
        &format!(
            "<!DOCTYPE html><body style='margin:0'>\
             <div style='font:10px/10px Ahem'>{}</div></body>",
            "<p style='margin:0'>xxxx</p>".repeat(400)
        ),
        PageOptions {
            viewport: Some(VIEWPORT),
            ..PageOptions::default()
        },
    )
    .unwrap();
    let boundaries =
        page.page_boundaries(PdfOptions::default().page_content_height(VIEWPORT.width));
    let pdf = page.print_to_pdf();
    let text = String::from_utf8_lossy(&pdf);

    // Each *page* stream is the one ending in `/Doc Do`; it opens with the clip
    // `x y w h re`. (The document form's own content is full of `re` operators
    // too, so the streams have to be picked out rather than the whole file
    // scanned.)
    let scale = PdfOptions::default().total_scale(VIEWPORT.width) * 0.75;
    let heights: Vec<f32> = text
        .match_indices("/Doc Do")
        .filter_map(|(at, _)| {
            let stream = text[..at].rsplit_once("stream\n")?.1;
            stream.split_whitespace().nth(3)?.parse::<f32>().ok()
        })
        .collect();
    assert_eq!(heights.len(), boundaries.len() - 1, "one clip per page");
    for (index, height) in heights.iter().enumerate() {
        let slice = (boundaries[index + 1] - boundaries[index]) * scale;
        assert!(
            (height - slice).abs() < 0.5,
            "page {index}: clip {height} != slice {slice}"
        );
    }
    // …and the slices really are shorter than a full page, or the test would
    // pass without the fix.
    let full = PdfOptions::default().content_box().1 * 0.75;
    assert!(
        heights.iter().any(|h| *h < full - 1.0),
        "no page stops short: {heights:?}"
    );
}
