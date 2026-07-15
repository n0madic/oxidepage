//! WP-B: paint-walk tests — stacking order, backgrounds, borders, overflow
//! clips, border-radius, visibility/opacity, and canvas propagation.

use oxidepage_base::Rect;
use oxidepage_dom::{ParseOptions, parse_document};
use oxidepage_layout::LayoutEngine;
use oxidepage_paint::{Brush, Color, DisplayItem, DisplayList, build_display_list};
use oxidepage_style::{StyleEngine, Viewport};

fn display_list(html: &str) -> DisplayList {
    let mut dom = parse_document(html, ParseOptions::default()).tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut engine = LayoutEngine::new(Viewport::default());
    engine.reflow(&mut dom, &mut style);
    build_display_list(&dom, &engine)
}

/// The solid-fill colors in paint order.
fn fill_colors(list: &DisplayList) -> Vec<Color> {
    list.items
        .iter()
        .filter_map(|item| match item {
            DisplayItem::Fill {
                brush: Brush::Solid(c),
                ..
            } => Some(*c),
            _ => None,
        })
        .collect()
}

/// The (rect, color) of every solid fill in paint order.
fn fills(list: &DisplayList) -> Vec<(Rect, Color)> {
    list.items
        .iter()
        .filter_map(|item| match item {
            DisplayItem::Fill {
                rect,
                brush: Brush::Solid(c),
                ..
            } => Some((*rect, *c)),
            _ => None,
        })
        .collect()
}

fn index_of(colors: &[Color], c: Color) -> usize {
    colors
        .iter()
        .position(|&x| x == c)
        .unwrap_or_else(|| panic!("color {c:?} not painted; got {colors:?}"))
}

const RED: Color = Color::rgb(255, 0, 0);
const GREEN: Color = Color::rgb(0, 255, 0);
const BLUE: Color = Color::rgb(0, 0, 255);

#[test]
fn base_is_opaque_white() {
    let list = display_list("<div></div>");
    // The very first item is the opaque-white canvas base.
    match &list.items[0] {
        DisplayItem::Fill {
            rect,
            brush: Brush::Solid(c),
            ..
        } => {
            assert_eq!(*c, Color::WHITE);
            assert_eq!(rect.origin.x, 0.0);
            assert_eq!(rect.origin.y, 0.0);
        }
        other => panic!("expected white base fill, got {other:?}"),
    }
}

#[test]
fn stacking_order_z_index() {
    let list = display_list(
        "<div style='position:relative'>\
           <div style='position:absolute;z-index:-1;background:#ff0000'></div>\
           <div style='background:#00ff00'>x</div>\
           <div style='position:absolute;z-index:1;background:#0000ff'></div>\
         </div>",
    );
    let colors = fill_colors(&list);
    let neg = index_of(&colors, RED);
    let flow = index_of(&colors, GREEN);
    let pos = index_of(&colors, BLUE);
    assert!(neg < flow, "z<0 paints below in-flow: {colors:?}");
    assert!(flow < pos, "z>0 paints above in-flow: {colors:?}");
}

#[test]
fn positioned_auto_z_paints_above_in_flow() {
    // A positioned element with z-index:auto paints in step 6, above the
    // in-flow siblings that precede it in the tree.
    let list = display_list(
        "<div style='position:relative'>\
           <div style='background:#00ff00'>x</div>\
           <div style='position:absolute;background:#0000ff'></div>\
         </div>",
    );
    let colors = fill_colors(&list);
    assert!(
        index_of(&colors, GREEN) < index_of(&colors, BLUE),
        "{colors:?}"
    );
}

#[test]
fn atomic_inline_in_positioned_span_stacks_with_parent() {
    // An <img> inside a `position: relative` span is a positioned descendant
    // (CSS 2.1 Appendix E): it paints with its positioned parent — above the
    // in-flow green sibling — not below it. The broken image paints a gray
    // placeholder, so its order is observable against the green background.
    let list = display_list(
        "<div style='position:relative'>\
           <span style='position:relative'>\
             <img style='width:10px;height:10px' src='http://example.com/x.png'>\
           </span>\
           <div style='background:#00ff00'>x</div>\
         </div>",
    );
    let gray = list
        .items
        .iter()
        .position(|i| {
            matches!(i,
            DisplayItem::Fill { brush: Brush::Solid(c), .. } if *c == Color::rgb(192, 192, 192))
        })
        .expect("img placeholder painted");
    let green = list
        .items
        .iter()
        .position(|i| {
            matches!(i,
            DisplayItem::Fill { brush: Brush::Solid(c), .. } if *c == GREEN)
        })
        .expect("green in-flow sibling painted");
    assert!(
        gray > green,
        "atomic inline in a relative span paints above the in-flow sibling: {:?}",
        list.items
    );
}

#[test]
fn overflow_hidden_emits_clip_pair() {
    let list = display_list(
        "<div style='overflow:hidden;width:50px;height:50px;border:5px solid #000'>x</div>",
    );
    let pushes = list
        .items
        .iter()
        .filter(|i| matches!(i, DisplayItem::PushClip { .. }))
        .count();
    let pops = list
        .items
        .iter()
        .filter(|i| matches!(i, DisplayItem::PopClip))
        .count();
    assert_eq!(pushes, 1, "one PushClip");
    assert_eq!(pops, 1, "one PopClip");

    // The clip is the padding box (border box minus the 5px border).
    let clip = list
        .items
        .iter()
        .find_map(|i| match i {
            DisplayItem::PushClip { rect, .. } => Some(*rect),
            _ => None,
        })
        .unwrap();
    assert_eq!(clip.size.width, 50.0);
    assert_eq!(clip.size.height, 50.0);
}

#[test]
fn border_radius_percentage_resolves_against_border_box() {
    let list = display_list(
        "<div style='width:100px;height:100px;border-radius:50%;background:#ff0000'></div>",
    );
    let radii = list
        .items
        .iter()
        .find_map(|i| match i {
            DisplayItem::Fill {
                brush: Brush::Solid(c),
                radii,
                ..
            } if *c == RED => Some(*radii),
            _ => None,
        })
        .expect("red fill present");
    // 50% of a 100×100 border box → 50px on every corner.
    assert!((radii.top_left.width - 50.0).abs() < 0.5, "{radii:?}");
    assert!((radii.top_left.height - 50.0).abs() < 0.5, "{radii:?}");
    assert!((radii.bottom_right.width - 50.0).abs() < 0.5, "{radii:?}");
}

#[test]
fn opacity_zero_skips_subtree() {
    let list = display_list("<div style='opacity:0;background:#ff0000'>x</div>");
    assert!(
        !fill_colors(&list).contains(&RED),
        "opacity:0 subtree must not paint"
    );
    assert!(
        !list
            .items
            .iter()
            .any(|i| matches!(i, DisplayItem::PushLayer { .. })),
        "opacity:0 emits no layer"
    );
}

#[test]
fn partial_opacity_emits_layer() {
    let list = display_list("<div style='opacity:0.5;background:#ff0000'>x</div>");
    let layer = list.items.iter().find_map(|i| match i {
        DisplayItem::PushLayer { opacity, .. } => Some(*opacity),
        _ => None,
    });
    assert_eq!(layer, Some(0.5));
    assert!(
        list.items
            .iter()
            .any(|i| matches!(i, DisplayItem::PopLayer))
    );
    assert!(fill_colors(&list).contains(&RED));
}

#[test]
fn visibility_hidden_skips_own_paint_but_descends() {
    let list = display_list(
        "<div style='visibility:hidden;background:#ff0000'>\
           <div style='visibility:visible;background:#00ff00'></div>\
         </div>",
    );
    let colors = fill_colors(&list);
    assert!(!colors.contains(&RED), "hidden box paints no background");
    assert!(colors.contains(&GREEN), "visible child still paints");
}

#[test]
fn canvas_background_propagates_from_body() {
    let list = display_list("<body style='background:#ff0000'><div>x</div></body>");
    let fills = fills(&list);
    // First fill: opaque-white base. Second: the propagated red canvas.
    assert_eq!(fills[0].1, Color::WHITE);
    assert_eq!(fills[1].1, RED);
    assert_eq!(fills[1].0.origin.x, 0.0);
    assert_eq!(fills[1].0.origin.y, 0.0);
    // The body must not paint its own background box a second time.
    let reds = fills.iter().filter(|(_, c)| *c == RED).count();
    assert_eq!(reds, 1, "body background propagated, not double-painted");
}

#[test]
fn html_background_wins_over_body_for_canvas() {
    let list = display_list(
        "<html style='background:#0000ff'><body style='background:#ff0000'><div>x</div></body></html>",
    );
    let fills = fills(&list);
    assert_eq!(fills[0].1, Color::WHITE);
    assert_eq!(fills[1].1, BLUE, "html background propagates to canvas");
    // Body still paints its own background on its own box.
    assert!(
        fills.iter().any(|(r, c)| *c == RED && r.origin.y > 0.0),
        "body paints its own background box: {fills:?}"
    );
}

#[test]
fn deeply_nested_boxes_do_not_overflow_the_stack() {
    // A box tree far deeper than any real document. The paint walk (paint_box →
    // paint_box_at → paint_box) recurses per level and would overflow the stack
    // without its depth cap. The build runs on a large-stack thread only so
    // that *layout* can construct the deep tree (layout's own recursion is out
    // of scope here); the assertion then proves the paint walk bounded itself,
    // stopping descent past the cap rather than painting all 700 levels.
    const NESTING: usize = 700;
    let reds = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            let mut html = String::new();
            for _ in 0..NESTING {
                html.push_str("<div style='background:#ff0000'>");
            }
            for _ in 0..NESTING {
                html.push_str("</div>");
            }
            // Completes without a paint-walk stack overflow / panic.
            let list = display_list(&html);
            fill_colors(&list).iter().filter(|&&c| c == RED).count()
        })
        .expect("spawn")
        .join()
        .expect("paint walk completes without overflow");

    // Descent was capped: not every nested box painted its red background.
    assert!(reds > 0, "some boxes still paint");
    assert!(
        reds < NESTING,
        "the deepest boxes past the cap are not painted (got {reds} of {NESTING})"
    );
}

#[test]
fn solid_border_emitted() {
    let list = display_list("<div style='width:40px;height:40px;border:3px solid #0000ff'></div>");
    let border = list.items.iter().find_map(|i| match i {
        DisplayItem::Border { edges, .. } => Some(*edges),
        _ => None,
    });
    let edges = border.expect("border emitted");
    for e in edges {
        assert_eq!(e.width, 3.0);
        assert_eq!(e.color, BLUE);
    }
}
