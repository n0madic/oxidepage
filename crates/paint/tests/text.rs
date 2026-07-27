//! WP-C: inline text painting — glyph positions, color, decorations,
//! debug text, and clipping inside `overflow: hidden`.

use oxidepage_dom::{ParseOptions, parse_document};
use oxidepage_layout::LayoutEngine;
use oxidepage_paint::{Brush, Color, DisplayItem, DisplayList, PaintOptions, build_display_list};
use oxidepage_style::{StyleEngine, Viewport};

fn display_list(html: &str) -> DisplayList {
    let mut dom = parse_document(html, ParseOptions::default()).tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut engine = LayoutEngine::new(Viewport::default());
    engine.reflow(&mut dom, &mut style);
    build_display_list(&dom, &engine, &PaintOptions::default())
}

fn first_glyph_run(list: &DisplayList) -> &DisplayItem {
    list.items
        .iter()
        .find(|i| matches!(i, DisplayItem::GlyphRun { .. }))
        .expect("a glyph run was painted")
}

const RED: Color = Color::rgb(255, 0, 0);

#[test]
fn ahem_glyph_positions_advance_by_em() {
    let list = display_list("<div style='font:16px Ahem'>XX</div>");
    let DisplayItem::GlyphRun { size, glyphs, .. } = first_glyph_run(&list) else {
        unreachable!()
    };
    assert_eq!(*size, 16.0);
    assert_eq!(glyphs.len(), 2, "two glyphs for \"XX\"");
    // Ahem glyphs advance one em (16px at 16px font size).
    let advance = glyphs[1].x - glyphs[0].x;
    assert!((advance - 16.0).abs() < 0.5, "advance was {advance}");
}

#[test]
fn text_color_from_span() {
    let list =
        display_list("<div style='font:16px Ahem'><span style='color:#ff0000'>XX</span></div>");
    let DisplayItem::GlyphRun { color, .. } = first_glyph_run(&list) else {
        unreachable!()
    };
    assert_eq!(*color, RED);
}

#[test]
fn debug_text_records_source() {
    let list = display_list("<div style='font:16px Ahem'>XX</div>");
    let DisplayItem::GlyphRun { debug_text, .. } = first_glyph_run(&list) else {
        unreachable!()
    };
    assert_eq!(debug_text.as_deref(), Some("XX"));
}

#[test]
fn underline_emits_decoration_fill() {
    let list = display_list(
        "<div style='font:16px Ahem'><span style='color:#ff0000;text-decoration:underline'>XX</span></div>",
    );
    // A thin red fill (the underline) distinct from any box background.
    let underline = list.items.iter().any(|i| {
        matches!(
            i,
            DisplayItem::Fill {
                brush: Brush::Solid(c),
                rect,
                ..
            } if *c == RED && rect.size.height > 0.0 && rect.size.height < 8.0
        )
    });
    assert!(underline, "underline fill present: {:?}", list.items);
}

#[test]
fn positioned_inline_text_stays_inside_intermediate_overflow_clip() {
    // A relative span's text escapes to its positioned ancestor's stacking
    // context (painted in the Positioned phase), but must remain clipped by the
    // `overflow: hidden` box sitting between them — whose main-walk clip pair is
    // already popped by the time that phase runs.
    let list = display_list(
        "<div style='position:relative'>\
           <div style='overflow:hidden;width:50px;height:16px;font:16px Ahem'>\
             <span style='position:relative'>XXXXXXXX</span>\
           </div>\
         </div>",
    );
    let glyph = list
        .items
        .iter()
        .position(|i| matches!(i, DisplayItem::GlyphRun { .. }))
        .expect("span text painted");
    // Net clip depth at the glyph run must be positive: it sits inside the
    // re-established overflow clip.
    let clip_depth: i32 = list.items[..glyph]
        .iter()
        .map(|i| match i {
            DisplayItem::PushClip { .. } => 1,
            DisplayItem::PopClip => -1,
            _ => 0,
        })
        .sum();
    assert!(
        clip_depth > 0,
        "positioned inline text must stay inside the overflow clip: {:?}",
        list.items
    );
    // And the clip closes again after the text.
    let closes = list.items[glyph..]
        .iter()
        .any(|i| matches!(i, DisplayItem::PopClip));
    assert!(closes, "overflow clip must be popped after the text");
}

#[test]
fn glyphs_clipped_inside_overflow_hidden() {
    let list =
        display_list("<div style='font:16px Ahem;overflow:hidden;width:8px;height:16px'>XX</div>");
    let push = list
        .items
        .iter()
        .position(|i| matches!(i, DisplayItem::PushClip { .. }))
        .expect("clip pushed");
    let glyph = list
        .items
        .iter()
        .position(|i| matches!(i, DisplayItem::GlyphRun { .. }))
        .expect("glyphs painted");
    let pop = list
        .items
        .iter()
        .position(|i| matches!(i, DisplayItem::PopClip))
        .expect("clip popped");
    assert!(
        push < glyph && glyph < pop,
        "glyphs must be inside the clip"
    );
}
