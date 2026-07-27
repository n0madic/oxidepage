//! CSS multi-column paint (ADR-0016): each column is a clip + translate view of
//! one continuous flow, so the flow subtree is emitted once per column.
//!
//! Ahem at `font-size: N; line-height: N` makes every glyph an N×N square and
//! every line exactly N tall, so the column boundaries are exact integers.

use oxidepage_base::{Rect, Transform2D};
use oxidepage_dom::{ParseOptions, parse_document};
use oxidepage_layout::LayoutEngine;
use oxidepage_paint::{DisplayItem, DisplayList, PaintOptions, build_display_list};
use oxidepage_style::{StyleEngine, Viewport};

fn display_list(html: &str) -> DisplayList {
    let mut dom = parse_document(html, ParseOptions::default()).tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut engine = LayoutEngine::new(Viewport::default());
    engine.reflow(&mut dom, &mut style);
    build_display_list(&dom, &engine, &PaintOptions::default())
}

fn clips(list: &DisplayList) -> Vec<Rect> {
    list.items
        .iter()
        .filter_map(|item| match item {
            DisplayItem::PushClip { rect, .. } => Some(*rect),
            _ => None,
        })
        .collect()
}

fn layer_transforms(list: &DisplayList) -> Vec<Transform2D> {
    list.items
        .iter()
        .filter_map(|item| match item {
            DisplayItem::PushLayer { transform, .. } => Some(*transform),
            _ => None,
        })
        .collect()
}

fn glyph_runs(list: &DisplayList) -> Vec<&str> {
    list.items
        .iter()
        .filter_map(|item| match item {
            DisplayItem::GlyphRun { debug_text, .. } => debug_text.as_deref(),
            _ => None,
        })
        .collect()
}

/// Balances into two 100px columns of two 20px Ahem lines each.
const TWO_COLUMNS: &str = "<body style='margin: 0'>\
     <div style='font-family: Ahem; font-size: 20px; line-height: 20px; width: 200px; \
     column-count: 2; column-gap: 0'>XXXXX XXXXX XXXXX XXXXX</div></body>";

#[test]
fn each_column_is_a_clipped_translated_view_of_the_flow() {
    let list = display_list(TWO_COLUMNS);

    // One clip per column, at the column's own slice height — *not* the used
    // column height: the strip below an early-terminated column holds the next
    // column's content and would otherwise show through.
    assert_eq!(
        clips(&list),
        [
            Rect::from_xywh(0.0, 0.0, 100.0, 40.0),
            Rect::from_xywh(100.0, 0.0, 100.0, 40.0),
        ]
    );

    // Column 0 needs no transform; column 1 is translated left-to-right and up
    // by the flow offset its slice starts at.
    assert_eq!(
        layer_transforms(&list),
        [Transform2D::translation(100.0, -40.0)]
    );

    // Clips and layers nest and balance: Clip(0) … PopClip, Clip(1) Layer …
    // PopLayer PopClip.
    let mut depth = 0i32;
    for item in &list.items {
        match item {
            DisplayItem::PushClip { .. } | DisplayItem::PushLayer { .. } => depth += 1,
            DisplayItem::PopClip | DisplayItem::PopLayer => depth -= 1,
            _ => {}
        }
        assert!(depth >= 0, "unbalanced clip/layer nesting");
    }
    assert_eq!(depth, 0);
}

#[test]
fn the_flow_is_emitted_once_per_column() {
    let single = display_list(
        "<body style='margin: 0'><div style='font-family: Ahem; font-size: 20px; \
         line-height: 20px; width: 100px'>XXXXX XXXXX XXXXX XXXXX</div></body>",
    );
    let columns = display_list(TWO_COLUMNS);

    // Same content, laid out at the same width — each column re-emits all of it,
    // and the clip keeps only its own slice.
    assert!(!glyph_runs(&single).is_empty());
    assert_eq!(glyph_runs(&columns).len(), 2 * glyph_runs(&single).len());
    assert_eq!(
        glyph_runs(&columns),
        [glyph_runs(&single).clone(), glyph_runs(&single)].concat()
    );
}

#[test]
fn a_relative_inline_paints_inside_its_column() {
    // Text in a `position: relative` inline paints in the Positioned phase, which
    // is normally emitted from the nearest positioned ancestor — outside the
    // column clip and transform. A multicol root is a barrier: it emits that pass
    // itself, once per column, inside the clip + layer.
    let list = display_list(
        "<body style='margin: 0'><div style='position: relative'>\
         <div style='font-family: Ahem; font-size: 20px; line-height: 20px; width: 200px; \
         column-count: 2; column-gap: 0'><span style='position: relative'>\
         XXXXX XXXXX XXXXX XXXXX</span></div></div></body>",
    );

    // Every glyph run sits inside a clip (and so inside a column).
    let mut clip_depth = 0i32;
    let mut runs_outside = 0;
    for item in &list.items {
        match item {
            DisplayItem::PushClip { .. } => clip_depth += 1,
            DisplayItem::PopClip => clip_depth -= 1,
            DisplayItem::GlyphRun { .. } if clip_depth == 0 => runs_outside += 1,
            _ => {}
        }
    }
    assert!(!glyph_runs(&list).is_empty());
    assert_eq!(runs_outside, 0, "positioned-inline text escaped its column");
}

#[test]
fn an_absolute_descendant_stays_in_the_flow_coordinate_space() {
    // The flow box is a containing block for every out-of-flow descendant, so an
    // absolutely positioned box resolves against the flow and is painted from its
    // static parent — *inside* the column layer, which relocates it exactly once.
    // Were it hoisted onto the `position: relative` ancestor outside, it would be
    // painted at an origin that already accounted for the column, and the layer
    // transform would then move it a second time.
    let list = display_list(
        "<body style='margin: 0'><div style='position: relative'>\
         <div style='width: 200px; column-count: 2; column-gap: 0'>\
         <div style='height: 60px'></div>\
         <div style='height: 60px'><span style='position: absolute; width: 10px; \
         height: 10px; background: red'></span></div></div></div></body>",
    );

    // Its static position is the top of the second block: flow y = 60, i.e. the
    // top of the second column. The flow is emitted once per column, so the box
    // is emitted twice — clipped away in column 1, translated into column 2.
    let mut transforms = Vec::new();
    let mut layer: Option<Transform2D> = None;
    for item in &list.items {
        match item {
            DisplayItem::PushLayer { transform, .. } => layer = Some(*transform),
            DisplayItem::PopLayer => layer = None,
            DisplayItem::Fill { rect, .. } if rect.size.width == 10.0 => {
                assert_eq!(
                    rect.origin,
                    oxidepage_base::Point::new(0.0, 60.0),
                    "the box must carry its flow-absolute origin, untransformed"
                );
                transforms.push(layer.unwrap_or(Transform2D::IDENTITY));
            }
            _ => {}
        }
    }
    assert_eq!(
        transforms,
        [
            Transform2D::IDENTITY,
            Transform2D::translation(100.0, -60.0)
        ]
    );
}
