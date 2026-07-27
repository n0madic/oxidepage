//! PDF pagination (ADR-0026): paper geometry, page count, the shared document
//! form XObject, fit-to-width, and the caps. Driven on hand-built display lists
//! like the rest of the suite; where the pages *break* is layout's job and is
//! tested in `crates/page/tests/pdf.rs`.

use oxidepage_base::{Rect, Size};
use oxidepage_export_pdf::{
    MAX_PDF_PAGES, Margins, PaperSize, PdfOptions, export, export_paginated,
};
use oxidepage_paint::{BorderRadii, Brush, Color, DisplayItem, DisplayList, ResourceTable};

/// An 800 × `content_h` document with one fill, so every page has something on
/// it and the output is never trivially empty.
fn list(content_h: f32) -> DisplayList {
    DisplayList {
        viewport: Size::new(800.0, 600.0),
        content_size: Size::new(800.0, content_h),
        items: vec![DisplayItem::Fill {
            rect: Rect::from_xywh(0.0, 0.0, 800.0, content_h),
            radii: BorderRadii::ZERO,
            brush: Brush::Solid(Color::rgb(255, 0, 0)),
        }],
        resources: ResourceTable::default(),
    }
}

fn text(pdf: &[u8]) -> String {
    String::from_utf8_lossy(pdf).into_owned()
}

/// The `/Count` of the page tree.
fn page_count(pdf: &[u8]) -> usize {
    let s = text(pdf);
    let at = s.find("/Count ").expect("page tree count");
    s[at + 7..]
        .split(|c: char| !c.is_ascii_digit())
        .find(|t| !t.is_empty())
        .expect("a number")
        .parse()
        .expect("a number")
}

fn media_boxes(pdf: &[u8]) -> Vec<String> {
    let s = text(pdf);
    s.match_indices("/MediaBox [")
        .map(|(at, _)| {
            let rest = &s[at + "/MediaBox [".len()..];
            rest[..rest.find(']').expect("closed")].to_owned()
        })
        .collect()
}

#[test]
fn a4_is_the_default_paper() {
    let pdf = export(&list(600.0), &PdfOptions::default());
    // 793.7 × 1122.52 CSS px × 0.75 = 595.275 × 841.89 pt — ISO A4 in points.
    let boxes = media_boxes(&pdf);
    assert_eq!(boxes.len(), 1, "one page for a 600px document");
    assert!(boxes[0].starts_with("0 0 595.27"), "{boxes:?}");
    assert!(boxes[0].ends_with("841.89"), "{boxes:?}");
}

#[test]
fn a_tall_document_spans_several_pages() {
    // A4 content box = 716.9 × 1045.72 px. The document is 800px wide, so
    // fit-to-width scales by 716.9/800 = 0.8961 and one page shows
    // 1045.72 / 0.8961 ≈ 1166.9 document px.
    let pdf = export(&list(5000.0), &PdfOptions::default());
    assert_eq!(page_count(&pdf), 5, "ceil(5000 / 1166.9)");
    assert_eq!(media_boxes(&pdf).len(), 5, "one media box per page");
}

#[test]
fn letter_and_landscape_change_the_media_box() {
    let pdf = export(
        &list(600.0),
        &PdfOptions {
            paper: PaperSize::LETTER,
            ..PdfOptions::default()
        },
    );
    // 816 × 1056 px × 0.75 = 612 × 792 pt.
    assert_eq!(media_boxes(&pdf), vec!["0 0 612 792".to_owned()]);

    let pdf = export(
        &list(600.0),
        &PdfOptions {
            paper: PaperSize::LETTER,
            landscape: true,
            ..PdfOptions::default()
        },
    );
    assert_eq!(media_boxes(&pdf), vec!["0 0 792 612".to_owned()]);
}

#[test]
fn paper_sizes_resolve_by_name() {
    assert_eq!(PaperSize::by_name("A4"), Some(PaperSize::A4));
    assert_eq!(PaperSize::by_name("letter"), Some(PaperSize::LETTER));
    assert_eq!(PaperSize::by_name("TABLOID"), Some(PaperSize::TABLOID));
    assert_eq!(PaperSize::by_name("foolscap"), None);
}

#[test]
fn margins_shift_the_content_and_shrink_the_content_box() {
    let wide = PdfOptions {
        margins: Margins::uniform(0.0),
        ..PdfOptions::default()
    };
    let narrow = PdfOptions {
        margins: Margins::uniform(100.0),
        ..PdfOptions::default()
    };
    assert_eq!(wide.content_box(), (793.7, 1122.52));
    let (w, h) = narrow.content_box();
    assert!(
        (w - 593.7).abs() < 0.01 && (h - 922.52).abs() < 0.01,
        "{w} {h}"
    );

    // The clip rect the page stream opens with starts at the left margin, in pt.
    let pdf = export(&list(600.0), &narrow);
    assert!(
        text(&pdf).contains("75 "),
        "left margin 100px = 75pt: clip origin"
    );

    // Under fit-to-width the scale follows the content *width*, so a wider
    // margin shrinks the document more — and therefore fits **more** of it on
    // each page, not less. (Turn fit-to-width off and the intuition flips back.)
    assert!(narrow.page_content_height(800.0) > wide.page_content_height(800.0));
    let fixed = |margins| PdfOptions {
        margins,
        fit_to_width: false,
        ..PdfOptions::default()
    };
    assert!(
        fixed(Margins::uniform(100.0)).page_content_height(800.0)
            < fixed(Margins::uniform(0.0)).page_content_height(800.0)
    );
    assert!(
        page_count(&export(&list(5000.0), &fixed(Margins::uniform(100.0))))
            > page_count(&export(&list(5000.0), &fixed(Margins::uniform(0.0))))
    );
}

#[test]
fn every_page_invokes_one_shared_document_form() {
    let pdf = export(&list(5000.0), &PdfOptions::default());
    let s = text(&pdf);
    assert_eq!(
        s.matches("/Subtype /Form").count(),
        1,
        "the document is emitted once, not once per page"
    );
    assert_eq!(
        s.matches("/Doc Do").count(),
        5,
        "and invoked by every page: {s}"
    );
}

#[test]
fn fit_to_width_shrinks_wide_content_and_never_magnifies() {
    let wide = DisplayList {
        viewport: Size::new(1280.0, 800.0),
        content_size: Size::new(1280.0, 800.0),
        items: Vec::new(),
        resources: ResourceTable::default(),
    };
    let options = PdfOptions::default();
    // A4 content width (793.7 − 2×38.4 = 716.9) / 1280 ≈ 0.5601.
    let scale = options.total_scale(wide.content_size.width);
    assert!((scale - 0.5601).abs() < 0.001, "{scale}");
    assert!(
        text(&export(&wide, &options)).contains("0.56"),
        "the page stream carries the fit scale"
    );

    // Narrow content is left alone rather than blown up.
    assert_eq!(options.total_scale(400.0), 1.0);
    // …and turning it off leaves the user scale alone.
    let off = PdfOptions {
        fit_to_width: false,
        ..options
    };
    assert_eq!(off.total_scale(1280.0), 1.0);
}

#[test]
fn user_scale_multiplies_the_fit_and_is_clamped() {
    let half = PdfOptions {
        scale: 0.5,
        ..PdfOptions::default()
    };
    assert_eq!(half.total_scale(400.0), 0.5);
    // Chrome's clamp: 0.1..=2.0.
    let absurd = PdfOptions {
        scale: 50.0,
        ..PdfOptions::default()
    };
    assert_eq!(absurd.total_scale(400.0), 2.0);
    let tiny = PdfOptions {
        scale: 0.0,
        ..PdfOptions::default()
    };
    assert_eq!(tiny.total_scale(400.0), 0.1);
    // A smaller scale fits more document per page.
    assert!(half.page_content_height(400.0) > PdfOptions::default().page_content_height(400.0));
}

#[test]
fn paginate_false_reproduces_the_single_tall_page() {
    let options = PdfOptions {
        paginate: false,
        ..PdfOptions::default()
    };
    let pdf = export(&list(5000.0), &options);
    assert_eq!(page_count(&pdf), 1);
    // 800 × 5000 CSS px × 0.75.
    assert_eq!(media_boxes(&pdf), vec!["0 0 600 3750".to_owned()]);
    // …and explicit boundaries cannot override the request.
    assert_eq!(
        export_paginated(&list(5000.0), &options, &[0.0, 1000.0, 5000.0]),
        pdf
    );
}

#[test]
fn explicit_boundaries_decide_where_pages_break() {
    let pdf = export_paginated(
        &list(3000.0),
        &PdfOptions::default(),
        &[0.0, 700.0, 1900.0, 3000.0],
    );
    assert_eq!(page_count(&pdf), 3);
}

#[test]
fn malformed_boundaries_are_sanitized() {
    let options = PdfOptions::default();
    // Out of order, duplicated, out of range and non-finite entries are dropped;
    // the span always stays [0, document height].
    let pdf = export_paginated(
        &list(3000.0),
        &options,
        &[
            0.0,
            1000.0,
            999.0,
            1000.0,
            f32::NAN,
            -50.0,
            9999.0,
            2000.0,
            3000.0,
        ],
    );
    assert_eq!(page_count(&pdf), 3, "0..1000, 1000..2000, 2000..3000");

    // No boundaries at all is one page covering the document.
    assert_eq!(
        page_count(&export_paginated(&list(3000.0), &options, &[])),
        1
    );
}

#[test]
fn the_page_count_is_capped() {
    // 10 million px at ~1164 px a page would be ~8600 pages.
    let pdf = export(&list(10_000_000.0), &PdfOptions::default());
    assert_eq!(page_count(&pdf), MAX_PDF_PAGES);
}

#[test]
fn a_wide_document_is_not_clipped_by_the_document_box() {
    // Fit-to-width measures against the content width, so the document box has
    // to be that wide too: a narrower box shrank the page to fit content that
    // the form XObject's `/BBox` then clipped away.
    let wide = DisplayList {
        viewport: Size::new(1280.0, 800.0),
        content_size: Size::new(2000.0, 800.0),
        items: Vec::new(),
        resources: ResourceTable::default(),
    };
    let pdf = export(&wide, &PdfOptions::default());
    let s = text(&pdf);
    // The form's BBox spans the full content width, in points.
    assert!(s.contains("/BBox [0 0 1500 600]"), "form bbox: {s}");
}

#[test]
fn each_page_clips_to_its_own_slice() {
    // Boundaries 700 apart on a 3000px document, against a ~1167px page: every
    // clip must be the slice, not the page's full content height, or the strip
    // below the break shows the next page's content twice.
    let pdf = export_paginated(
        &list(3000.0),
        &PdfOptions::default(),
        &[0.0, 700.0, 1400.0, 3000.0],
    );
    let s = text(&pdf);
    let scale = PdfOptions::default().total_scale(800.0) * 0.75;
    let heights: Vec<f32> = s
        .match_indices("/Doc Do")
        .filter_map(|(at, _)| {
            let stream = s[..at].rsplit_once("stream\n")?.1;
            stream.split_whitespace().nth(3)?.parse::<f32>().ok()
        })
        .collect();
    assert_eq!(heights.len(), 3);
    assert!((heights[0] - 700.0 * scale).abs() < 0.5, "{heights:?}");
    assert!((heights[1] - 700.0 * scale).abs() < 0.5, "{heights:?}");
    // The last slice (1600px) is taller than a page, so the page bounds it.
    let full = PdfOptions::default().content_box().1 * 0.75;
    assert!((heights[2] - full).abs() < 0.5, "{heights:?}");
}
