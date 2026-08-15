//! The UA limit on overlarge grids: `taffy_style_for()`'s guarded conversion
//! and per-axis track clamp (CSS Grid 2 §"Limiting Large Grids").
//!
//! Every assertion is on layout output, never on wall-clock time — the point of
//! the clamp is that the work is bounded, but a timing assertion flakes on a
//! loaded machine. An unbounded grid shows up here as a track count (read off
//! an `inline-grid`'s shrink-to-fit width) or as a wrapped, out-of-place item,
//! and both are exact.

use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};
use oxidepage_layout::LayoutEngine;
use oxidepage_style::{StyleEngine, Viewport};

/// Mirrors `MAX_GRID_TRACKS_PER_AXIS` in `construct.rs`.
const MAX_TRACKS: f32 = 1000.0;

fn find_by_id(tree: &DomTree, id_attr: &str) -> NodeId {
    tree.inclusive_descendants(tree.document())
        .find(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| el.id().map(|a| &**a) == Some(id_attr))
        })
        .unwrap_or_else(|| panic!("no element with id={id_attr}"))
}

fn setup(html: &str) -> (DomTree, StyleEngine, LayoutEngine) {
    let mut dom = parse_document(html, ParseOptions::default()).tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut layout = LayoutEngine::new(Viewport::default());
    layout
        .reflow(&mut dom, &mut style)
        .expect("layout completes");
    (dom, style, layout)
}

/// The width of an `inline-grid` is the sum of its tracks, so it reads back the
/// number of tracks the conversion actually generated.
fn inline_grid_width(grid_style: &str, item: &str) -> f32 {
    let html = format!(
        "<body style='margin: 0'>\
         <div id=grid style='display: inline-grid; {grid_style}'>{item}</div></body>"
    );
    let (dom, _s, layout) = setup(&html);
    let grid = find_by_id(&dom, "grid");
    layout
        .border_box(grid)
        .expect("grid container generates a box")
        .size
        .width
}

#[test]
fn control_grid_under_the_cap_is_untouched() {
    // The regression guard that matters: the clamp is a ceiling, so a grid
    // within the limit must lay out exactly as it did before it existed.
    // 12 x 20px columns, one item per column, all placed explicitly.
    let items: String = (1..=12)
        .map(|column| {
            format!(
                "<i style='grid-column: {column} / {}; height: 5px'></i>",
                column + 1
            )
        })
        .collect();
    assert_eq!(
        inline_grid_width("grid-template-columns: repeat(12, 20px)", &items),
        240.0
    );
}

#[test]
fn huge_repeat_count_does_not_panic() {
    // `repeat(70000, 1px)` overflows the `u16` that
    // `stylo_taffy::convert::track_repeat` unwraps into: before the guard this
    // panicked out of `LayoutEngine::reflow` and took the thread with it.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=grid style='display: grid; grid-template-columns: repeat(70000, 1px)'>\
         <i id=item>x</i></div></body>",
    );
    assert!(layout.border_box(find_by_id(&dom, "grid")).is_some());
    assert!(layout.border_box(find_by_id(&dom, "item")).is_some());
}

#[test]
fn huge_named_span_does_not_panic() {
    // The other `try_into().unwrap()`: a named span's count in
    // `stylo_taffy::convert::grid_line`.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: grid; grid-template-columns: [foo] 10px [foo] 10px'>\
         <i id=item style='grid-column: span foo 70000'>x</i></div></body>",
    );
    assert!(layout.border_box(find_by_id(&dom, "item")).is_some());
}

#[test]
fn giving_up_on_a_template_keeps_the_container_placed() {
    // A container whose own template cannot be converted through upstream
    // still has a valid placement in *its* parent grid.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: grid; width: 300px; grid-template-columns: 100px 100px 100px'>\
         <div id=inner style='grid-column: 2 / 3; \
         grid-template-columns: repeat(70000, 1px); display: grid'></div></div></body>",
    );
    let inner = find_by_id(&dom, "inner");
    assert_eq!(layout.border_box(inner).unwrap().origin.x, 100.0);
}

#[test]
fn the_saturating_mirror_keeps_every_non_grid_property() {
    // The mirror exists because `to_taffy_style` is one struct literal: a panic
    // inside it loses `position`/`inset`/`size` along with the grid values, and
    // `LayoutBox::position` — captured from stylo separately — would still say
    // `absolute` while taffy laid the box out in flow. The container below must
    // land exactly where the same markup with a representable template does.
    let template = |columns: &str| {
        format!(
            "<body style='margin: 0'>\
             <div style='position: relative; width: 300px; height: 200px'>\
             <div id=abs style='position: absolute; left: 50px; top: 20px; \
             width: 100px; height: 30px; display: grid; grid-template-columns: {columns}'></div>\
             <div id=sib style='height: 10px'></div></div></body>"
        )
    };
    for columns in ["repeat(3, 1px)", "repeat(70000, 1px)"] {
        let (dom, _s, layout) = setup(&template(columns));
        let rect = layout.border_box(find_by_id(&dom, "abs")).unwrap();
        assert_eq!(
            (
                rect.origin.x,
                rect.origin.y,
                rect.size.width,
                rect.size.height
            ),
            (50.0, 20.0, 100.0, 30.0),
            "out-of-flow geometry lost with grid-template-columns: {columns}"
        );
    }
}

#[test]
fn explicit_track_count_is_clamped_per_axis() {
    // Both sides of the `u16` that upstream's `repeat()` count narrows into:
    // 20000 converts and is clamped, 70000 would panic the conversion and goes
    // through the saturating mirror. Neither may fall off a cliff at 65536 —
    // the limit is one rule, not two.
    for repeat in ["repeat(20000, 1px)", "repeat(70000, 1px)"] {
        assert_eq!(
            inline_grid_width(&format!("grid-template-columns: {repeat}"), ""),
            MAX_TRACKS,
            "{repeat} did not clamp to the track limit"
        );
    }
}

#[test]
fn the_track_budget_spans_the_whole_axis() {
    // A per-`repeat()` cap is evaded by splitting the repetition in two, and by
    // a bare list of single tracks. Both stay under the axis budget.
    assert_eq!(
        inline_grid_width(
            "grid-template-columns: repeat(600, 1px) repeat(600, 1px)",
            ""
        ),
        MAX_TRACKS
    );
    let singles = "1px ".repeat(1200);
    assert_eq!(
        inline_grid_width(&format!("grid-template-columns: {singles}"), ""),
        MAX_TRACKS
    );
}

#[test]
fn a_far_line_clamps_instead_of_wrapping() {
    // `grid-column: 1 / 100000` narrows to `-31072` under upstream's `as i16`,
    // which places the item at an unrelated negative line rather than a large
    // one. Clamped, the item spans the capped number of implicit tracks.
    let width = inline_grid_width(
        "grid-auto-columns: 1px",
        "<i style='grid-column: 1 / 100000'></i>",
    );
    assert_eq!(width, MAX_TRACKS);
}

#[test]
fn a_far_negative_line_clamps_instead_of_wrapping() {
    // The same narrowing turns `-100000` into a *positive* `31072`, so an
    // unclamped grid generates ~31000 implicit tracks from a value that asks
    // for tracks before the explicit grid.
    let width = inline_grid_width(
        "grid-auto-columns: 1px",
        "<i style='grid-column: -100000 / 2'></i>",
    );
    assert!(
        width <= MAX_TRACKS + 2.0,
        "far negative line generated {width} tracks worth of columns"
    );
}

#[test]
fn a_huge_span_is_clamped() {
    let width = inline_grid_width(
        "grid-auto-columns: 1px",
        "<i style='grid-column: span 50000'></i>",
    );
    assert_eq!(width, MAX_TRACKS);
}
