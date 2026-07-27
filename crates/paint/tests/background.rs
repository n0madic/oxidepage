//! WP-E: gradient background tests — linear endpoints, radial farthest-corner,
//! explicit size, and background-position.

use oxidepage_dom::{ParseOptions, parse_document};
use oxidepage_layout::LayoutEngine;
use oxidepage_paint::{
    Brush, Color, DisplayItem, DisplayList, LinearGradient, PaintOptions, RadialGradient,
    build_display_list,
};
use oxidepage_style::{StyleEngine, Viewport};

fn display_list(html: &str) -> DisplayList {
    let mut dom = parse_document(html, ParseOptions::default()).tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut engine = LayoutEngine::new(Viewport::default());
    engine.reflow(&mut dom, &mut style);
    build_display_list(&dom, &engine, &PaintOptions::default())
}

fn linear(list: &DisplayList) -> LinearGradient {
    list.items
        .iter()
        .find_map(|i| match i {
            DisplayItem::Fill {
                brush: Brush::LinearGradient(g),
                ..
            } => Some(g.clone()),
            _ => None,
        })
        .expect("a linear gradient was painted")
}

fn radial(list: &DisplayList) -> RadialGradient {
    list.items
        .iter()
        .find_map(|i| match i {
            DisplayItem::Fill {
                brush: Brush::RadialGradient(g),
                ..
            } => Some(g.clone()),
            _ => None,
        })
        .expect("a radial gradient was painted")
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.5
}

#[test]
fn linear_to_right_endpoints_span_width() {
    let list = display_list(
        "<body style='margin:0'>\
           <div style='width:100px;height:50px;background:linear-gradient(to right,#ff0000,#0000ff)'></div>\
         </body>",
    );
    let g = linear(&list);
    assert!(
        close(g.start.x, 0.0) && close(g.start.y, 25.0),
        "start {:?}",
        g.start
    );
    assert!(
        close(g.end.x, 100.0) && close(g.end.y, 25.0),
        "end {:?}",
        g.end
    );
    // Two stops, red at 0, blue at 1.
    assert_eq!(g.stops.len(), 2);
    assert_eq!(g.stops[0].color, Color::rgb(255, 0, 0));
    assert!(close(g.stops[0].offset, 0.0));
    assert_eq!(g.stops[1].color, Color::rgb(0, 0, 255));
    assert!(close(g.stops[1].offset, 1.0));
}

#[test]
fn linear_to_top_endpoints() {
    let list = display_list(
        "<body style='margin:0'>\
           <div style='width:40px;height:80px;background:linear-gradient(to top,#ff0000,#0000ff)'></div>\
         </body>",
    );
    let g = linear(&list);
    // 'to top': start at bottom-center, end at top-center.
    assert!(
        close(g.start.x, 20.0) && close(g.start.y, 80.0),
        "start {:?}",
        g.start
    );
    assert!(
        close(g.end.x, 20.0) && close(g.end.y, 0.0),
        "end {:?}",
        g.end
    );
}

#[test]
fn radial_farthest_corner_default_ellipse() {
    let list = display_list(
        "<body style='margin:0'>\
           <div style='width:100px;height:100px;background:radial-gradient(#ff0000,#0000ff)'></div>\
         </body>",
    );
    let g = radial(&list);
    assert!(
        close(g.center.x, 50.0) && close(g.center.y, 50.0),
        "center {:?}",
        g.center
    );
    // Default ellipse, farthest-corner, centered in 100×100:
    // radius = sqrt(2) * 50 ≈ 70.71 on each axis.
    assert!(close(g.radius.width, 70.71), "rx {}", g.radius.width);
    assert!(close(g.radius.height, 70.71), "ry {}", g.radius.height);
}

#[test]
fn explicit_size_no_repeat_positions_tile() {
    // A 50×50 gradient tile at 100% 100% inside a 100×100 area sits at (50,50).
    let list = display_list(
        "<body style='margin:0'>\
           <div style='width:100px;height:100px;\
                       background:linear-gradient(to right,#ff0000,#0000ff);\
                       background-size:50px 50px;background-repeat:no-repeat;\
                       background-position:100% 100%'></div>\
         </body>",
    );
    let g = linear(&list);
    // Tile origin (50,50); 'to right' endpoints span the tile's 50px width at
    // its vertical center (50 + 25 = 75).
    assert!(
        close(g.start.x, 50.0) && close(g.start.y, 75.0),
        "start {:?}",
        g.start
    );
    assert!(
        close(g.end.x, 100.0) && close(g.end.y, 75.0),
        "end {:?}",
        g.end
    );
}

#[test]
fn cover_fills_area() {
    let list = display_list(
        "<body style='margin:0'>\
           <div style='width:120px;height:60px;\
                       background:linear-gradient(to right,#ff0000,#0000ff);\
                       background-size:cover'></div>\
         </body>",
    );
    let g = linear(&list);
    // cover → tile equals the positioning area (120×60), so 'to right'
    // endpoints span the full width.
    assert!(close(g.start.x, 0.0), "start {:?}", g.start);
    assert!(close(g.end.x, 120.0), "end {:?}", g.end);
}
