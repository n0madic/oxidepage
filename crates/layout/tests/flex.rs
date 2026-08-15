//! Flexbox WPT-tail fixes: `taffy_style_for()` seam fixes (`width: stretch`,
//! RTL-aware `justify-content: left|right`, `safe`/`unsafe`
//! overflow-alignment), `first_baselines` wiring for `align-items: baseline`,
//! and the `order` property.

use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};
use oxidepage_layout::LayoutEngine;
use oxidepage_style::{StyleEngine, Viewport};

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

#[test]
fn width_stretch_fills_containing_block() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; width: 200px'>\
         <div id=item style='width: stretch; height: 20px'></div></div></body>",
    );
    let item = find_by_id(&dom, "item");
    let rect = layout.border_box(item).unwrap();
    assert_eq!(rect.size.width, 200.0);
}

#[test]
fn justify_content_left_right_respect_rtl() {
    // `left`/`right` are physical keywords: the item must land in the same
    // physical position regardless of `direction`. Before the fix,
    // `content_alignment()` mapped them to taffy's logical `Start`/`End`
    // unconditionally, so RTL flipped a row flex container's main axis and
    // silently moved `left` to the physical right edge.
    for direction in ["ltr", "rtl"] {
        let html = format!(
            "<body style='margin: 0'>\
             <div style='display: flex; width: 100px; direction: {direction}; justify-content: left'>\
             <div id=item style='width: 20px; height: 20px'></div></div></body>"
        );
        let (dom, _s, layout) = setup(&html);
        let item = find_by_id(&dom, "item");
        let x = layout.border_box(item).unwrap().origin.x;
        assert_eq!(
            x, 0.0,
            "justify-content:left in {direction} should hug the physical left edge"
        );
    }

    for direction in ["ltr", "rtl"] {
        let html = format!(
            "<body style='margin: 0'>\
             <div style='display: flex; width: 100px; direction: {direction}; justify-content: right'>\
             <div id=item style='width: 20px; height: 20px'></div></div></body>"
        );
        let (dom, _s, layout) = setup(&html);
        let item = find_by_id(&dom, "item");
        let x = layout.border_box(item).unwrap().origin.x;
        assert_eq!(
            x, 80.0,
            "justify-content:right in {direction} should hug the physical right edge"
        );
    }
}

#[test]
fn safe_center_falls_back_when_overflowing() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; align-items: safe center; width: 100px; height: 50px'>\
         <div id=item style='width: 20px; height: 100px'></div></div></body>",
    );
    let item = find_by_id(&dom, "item");
    let rect = layout.border_box(item).unwrap();
    // Overflowing the cross axis: `safe center` falls back to start, not centered
    // (which would place the item at y = -25).
    assert_eq!(rect.origin.y, 0.0);
}

#[test]
fn flex_align_items_baseline_aligns_by_first_line_ascent() {
    // Before the fix, `inline.rs`'s IFC leaf unconditionally reported
    // `first_baselines: Point::NONE`, so `align-items: baseline` degraded to
    // flex-end-like (bottom-edge) alignment instead of aligning by ascent.
    // Ahem's ascent is 0.8em; with `line-height: 1` (no extra leading), the
    // line box's own ascent above its top is exactly 0.8 * font-size.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; align-items: baseline'>\
         <div id=a style='font: 30px/1 Ahem'>x</div>\
         <div id=b style='font: 20px/1 Ahem'>x</div>\
         </div></body>",
    );
    let a_top = layout.border_box(find_by_id(&dom, "a")).unwrap().origin.y;
    let b_top = layout.border_box(find_by_id(&dom, "b")).unwrap().origin.y;
    assert_eq!(a_top + 0.8 * 30.0, b_top + 0.8 * 20.0);
}

#[test]
fn multicol_leaf_reports_first_line_baseline() {
    // Mirrors the previous test, but the second item is a multicol container
    // (its own `first_baselines` comes from `multicol.rs`'s flow child, not
    // directly from an IFC leaf).
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; align-items: baseline'>\
         <div id=a style='font: 30px/1 Ahem'>x</div>\
         <div id=b style='column-count: 2; column-gap: 0; width: 40px; font: 20px/1 Ahem'>x</div>\
         </div></body>",
    );
    let a_top = layout.border_box(find_by_id(&dom, "a")).unwrap().origin.y;
    let b_top = layout.border_box(find_by_id(&dom, "b")).unwrap().origin.y;
    assert_eq!(a_top + 0.8 * 30.0, b_top + 0.8 * 20.0);
}

#[test]
fn flex_order_reorders_layout_and_paint() {
    // Taffy has no `order` field — it lays out `children` in tree order.
    // `collect_flex_grid_children` must pre-sort by `order` so the second
    // DOM child (order: 1) lays out first (flush left) despite coming after
    // the first DOM child (order: 2).
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; width: 100px'>\
         <div id=first style='order: 2; width: 20px; height: 20px'></div>\
         <div id=second style='order: 1; width: 20px; height: 20px'></div>\
         </div></body>",
    );
    let first = find_by_id(&dom, "first");
    let second = find_by_id(&dom, "second");
    assert_eq!(layout.border_box(second).unwrap().origin.x, 0.0);
    assert_eq!(layout.border_box(first).unwrap().origin.x, 20.0);
}

#[test]
fn flex_order_affects_hit_test_paint_order() {
    // Mirrors hittest-overlapping-order.html: the DOM-first, `order: 1` item
    // paints last (on top) because `order` also drives `hit_box`'s paint-order
    // tiebreaker (via the same sorted `children`), so it wins the overlap.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; width: 600px'>\
         <div id=right style='order: 1; width: 300px; height: 20px; \
         margin-left: -100px; background: green'></div>\
         <div id=left style='order: 0; width: 300px; height: 20px; \
         background: gray'></div>\
         </div></body>",
    );
    let right = find_by_id(&dom, "right");
    // (250, 5) is inside the overlap of both boxes; `right` paints on top.
    assert_eq!(layout.element_from_point(&dom, 250.0, 5.0), Some(right));
}

#[test]
fn flex_basis_min_content_shrinks_to_content() {
    // `stylo_taffy::convert::dimension()` collapses `min-content` to `AUTO`
    // (upstream TODO). `intrinsic_size::resolve_intrinsic_size_keywords` must
    // measure it and overwrite `style.flex_basis` before taffy's real layout
    // pass runs. Two floated 50px children never wrap, so min-content width
    // is the wider one: 50px.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; width: 75px; height: 75px'>\
         <div id=item style='flex-basis: min-content; flex-shrink: 0; min-width: 0'>\
         <div style='float: left; width: 50px; height: 50px'></div>\
         <div style='float: left; width: 50px; height: 50px'></div>\
         </div></div></body>",
    );
    let item = find_by_id(&dom, "item");
    assert_eq!(layout.border_box(item).unwrap().size.width, 50.0);
}

#[test]
fn width_max_content_ignores_specified_width() {
    // `flex-basis`, when not `auto`/`content`, is authoritative over a
    // separately-set `width` on the same flex item (CSS Flexbox §9.2).
    // Resolving `flex-basis: max-content` must use `SizingMode::ContentSize`
    // (content only), not `InherentSize` (which would read the item's own
    // `width: 300px` as its "inherent size" and return that unchanged instead
    // of the true max-content width of its children: 50 + 50 = 100px).
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; width: 75px; height: 75px'>\
         <div id=item style='flex-basis: max-content; width: 300px; \
         flex-shrink: 0; min-width: 0'>\
         <div style='float: left; width: 50px; height: 50px'></div>\
         <div style='float: left; width: 50px; height: 50px'></div>\
         </div></div></body>",
    );
    let item = find_by_id(&dom, "item");
    assert_eq!(layout.border_box(item).unwrap().size.width, 100.0);
}

#[test]
fn replaced_min_content_contribution_zeroes_percentage_part() {
    // CSS Sizing 3 §replaced-percentage-min-contribution: a replaced element's
    // min-content contribution resolves a percentage in that axis against
    // *zero*, so `calc(140px + 100%)` contributes 140px. That contribution is
    // what taffy's automatic-minimum-size clamp floors the item at, so the
    // `<input>` must not shrink past 140 even though only 100px is left over
    // (300 container - 200 spacer). The form-control leaf used to ignore
    // `style.size` entirely and report 0, letting it shrink to 100.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; width: 300px; height: 40px'>\
         <div style='flex: 0 0 200px'></div>\
         <input id=inp style='font: 20px/1 Ahem; border: 0; padding: 0; margin: 0; \
         width: calc(140px + 100%)'>\
         </div></body>",
    );
    let inp = find_by_id(&dom, "inp");
    assert_eq!(layout.border_box(inp).unwrap().size.width, 140.0);
}

#[test]
fn flex_item_own_height_does_not_inflate_its_content_size_suggestion() {
    // CSS Flexbox §4.5: a column flex item's own height must not influence its
    // content size suggestion, so the `height: 100%` child measures against an
    // indefinite basis (i.e. as `auto`) and the item keeps its 100px
    // flex-basis. Taffy folds `size.height` into the percentage basis it hands
    // children (`container_percentage_resolution_height`) even under
    // `SizingMode::ContentSize`, which resolved the child against 200px and
    // dragged the automatic minimum size — and the item — back up to 200.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; flex-direction: column; width: 100px'>\
         <div id=item style='flex-basis: 100px; height: 200px'>\
         <div style='height: 100%'></div>\
         </div></div></body>",
    );
    let item = find_by_id(&dom, "item");
    assert_eq!(layout.border_box(item).unwrap().size.height, 100.0);
}

#[test]
fn definite_flex_basis_caps_the_containers_intrinsic_main_size() {
    // What Gecko/Blink/WebKit ship, and what
    // `flex-one-sets-flex-basis-to-zero-px.html` asserts (in as many words):
    // a definite `flex-basis` caps the item's intrinsic main-size contribution
    // whatever the grow factor, so an auto-height column flex container around
    // `flex: 1 1 0px` collapses to 0. CSS Flexbox §9.9.1 to the letter would
    // instead hand back the item's 14px max-content height — which is what
    // taffy does, since it only applies that cap when `flex-grow` is 0.
    for flex in ["1 1 0px", "0.5 1 0px"] {
        let html = format!(
            "<body style='margin: 0'>\
             <div style='display: flex; flex-direction: column'>\
             <div id=item style='flex: {flex}; min-height: 0; font: 14px/1 Ahem'>x</div>\
             </div></body>"
        );
        let (dom, _s, layout) = setup(&html);
        let item = find_by_id(&dom, "item");
        assert_eq!(
            layout.border_box(item).unwrap().size.height,
            0.0,
            "`flex: {flex}` must collapse, not grow to its max-content height"
        );
    }
}

#[test]
fn definite_flex_basis_still_grows_into_a_sized_container() {
    // The other side of the latch above: the cap is only sound while the
    // container's main size is being *derived* from its items. A container with
    // a real main size distributes free space as usual, and the item must grow
    // into it — 40px here, not the 30px flex basis
    // (`image-as-flexitem-size-006.html`, which the first cut of this broke).
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; flex-direction: column; height: 40px'>\
         <div id=item style='flex: 1 1 30px; min-height: 0'></div>\
         </div></body>",
    );
    let item = find_by_id(&dom, "item");
    assert_eq!(layout.border_box(item).unwrap().size.height, 40.0);
}

#[test]
fn align_items_baseline_scroll_container_uses_bottom_margin_edge() {
    // CSS 2.1 §10.8.1's `inline-block` baseline rule (shared by flex/grid's
    // synthesized-baseline fallback): once `overflow` computes to anything
    // but `visible`, the baseline is the bottom margin edge, not the first
    // line's baseline. Ahem's ascent is 0.8em; the scroller's own height
    // (50px) becomes the shared group baseline (the larger of the two), so
    // the plain text item is pushed down by 50 - 0.8*20 = 34px to match it.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='display: flex; align-items: baseline'>\
         <div id=scroller style='overflow: auto; height: 50px; font: 20px/1 Ahem'>text</div>\
         <div id=plain style='font: 20px/1 Ahem'>text</div>\
         </div></body>",
    );
    let scroller_top = layout
        .border_box(find_by_id(&dom, "scroller"))
        .unwrap()
        .origin
        .y;
    let plain_top = layout
        .border_box(find_by_id(&dom, "plain"))
        .unwrap()
        .origin
        .y;
    assert_eq!(scroller_top, 0.0);
    assert_eq!(plain_top, 34.0);
}
