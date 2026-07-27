//! WP-E: geometry service tests — coordinate accumulation with scroll,
//! per-line inline rects, scroll clamping, and hit testing. Metric-dependent
//! cases use Ahem.

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
    layout.reflow(&mut dom, &mut style);
    (dom, style, layout)
}

#[test]
fn nested_block_positions_accumulate() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='padding: 10px; margin: 5px'>\
         <div id=inner style='width: 50px; height: 20px'></div></div></body>",
    );
    let inner = find_by_id(&dom, "inner");
    let rect = layout.border_box(inner).unwrap();
    // body margin 0 → outer div at (5,5); + padding 10 → inner at (15,15).
    assert_eq!((rect.origin.x, rect.origin.y), (15.0, 15.0));
    assert_eq!((rect.size.width, rect.size.height), (50.0, 20.0));
}

#[test]
fn scroll_offset_shifts_descendant_rects() {
    let (dom, _s, mut layout) = setup(
        "<body style='margin: 0'>\
         <div id=scroller style='overflow: scroll; width: 100px; height: 100px'>\
         <div id=content style='width: 100px; height: 500px'></div></div></body>",
    );
    let scroller = find_by_id(&dom, "scroller");
    let content = find_by_id(&dom, "content");

    let before = layout.border_box(content).unwrap();
    assert_eq!(before.origin.y, 0.0);

    let result = layout.set_scroll_offset(scroller, 0.0, 150.0);
    assert!(result.changed);
    assert_eq!(result.y, 150.0);

    let after = layout.border_box(content).unwrap();
    assert_eq!(after.origin.y, -150.0);
    // The scroller itself doesn't move.
    assert_eq!(layout.border_box(scroller).unwrap().origin.y, 0.0);
}

#[test]
fn scroll_offset_clamps_to_overflow() {
    let (dom, _s, mut layout) = setup(
        "<div id=scroller style='overflow: scroll; width: 100px; height: 100px'>\
         <div style='width: 100px; height: 250px'></div></div>",
    );
    let scroller = find_by_id(&dom, "scroller");

    // Max scroll = 250 - 100 = 150.
    let result = layout.set_scroll_offset(scroller, 0.0, 10_000.0);
    assert_eq!(result.y, 150.0);
    // Negative clamps to zero, and reports changed.
    let result = layout.set_scroll_offset(scroller, -5.0, -5.0);
    assert_eq!((result.x, result.y), (0.0, 0.0));
    assert!(result.changed);
    // Same value again: no change.
    let result = layout.set_scroll_offset(scroller, 0.0, 0.0);
    assert!(!result.changed);

    // Non-scroll-containers clamp everything to zero.
    let (dom2, _s2, mut layout2) = setup("<div id=plain><div style='height: 50px'></div></div>");
    let plain = find_by_id(&dom2, "plain");
    let result = layout2.set_scroll_offset(plain, 10.0, 10.0);
    assert_eq!((result.x, result.y), (0.0, 0.0));
}

#[test]
fn align_items_flex_end_does_not_inflate_scroll_height_upward() {
    // Mirrors negative-overflow.html subtest 1: a flex item taller than its
    // scrollable container, pinned to the cross-end (bottom), overflows
    // *above* the container. That start-ward excess must not count toward
    // scrollHeight — only end-ward overflow is reachable by scrolling.
    let (dom, _s, layout) = setup(
        "<div id=c style='display: flex; align-items: flex-end; overflow: auto; \
         width: 50px; height: 50px'>\
         <div style='width: 100%; height: 100px'></div></div>",
    );
    let c = find_by_id(&dom, "c");
    let (_, height) = layout.scroll_size(c).unwrap();
    assert_eq!(height, 50.0);
}

#[test]
fn align_items_center_scroll_height_includes_trailing_padding() {
    // Mirrors negative-overflow.html subtest 7: `align-items: center` also
    // ignores its start-ward excess, but CSS Overflow's "trailing padding"
    // rule still adds one padding-bottom past the item's actual (unclamped)
    // end edge, so scrolling to the end reveals the full bottom padding.
    let (dom, _s, layout) = setup(
        "<div id=c style='display: flex; align-items: center; overflow: auto; \
         width: 50px; height: 50px; padding-top: 5px; padding-bottom: 10px'>\
         <div style='width: 100%; height: 100px'></div></div>",
    );
    let c = find_by_id(&dom, "c");
    let (_, height) = layout.scroll_size(c).unwrap();
    assert_eq!(height, 90.0);
}

#[test]
fn justify_content_rtl_row_overflow_excludes_start_side() {
    // Mirrors negative-overflow-002.html's horizontal-tb/rtl/row/nowrap case:
    // in RTL, the flex main-start is the physical right edge, so an item
    // wider than its container overflows toward physical *negative* x. That
    // is the logical end (not start) of an RTL main axis, so it must count
    // toward scrollWidth — the opposite of the LTR default.
    let (dom, _s, layout) = setup(
        "<div id=c style='display: flex; direction: rtl; overflow: auto; \
         width: 50px; height: 20px; padding-left: 5px; padding-right: 10px'>\
         <div style='width: 100px; height: 20px; flex-shrink: 0'></div></div>",
    );
    let c = find_by_id(&dom, "c");
    let (width, _) = layout.scroll_size(c).unwrap();
    // Content reaches 100px past the RTL start (physical right); the trailing
    // bonus adds one more padding-left past the item's negative-x end edge.
    assert_eq!(width, 115.0);
}

#[test]
fn negative_margin_sibling_does_not_suppress_another_childs_overflow() {
    // A child with a purely margin-driven overhang (`margin: 0 -5px` widening
    // an auto-width block, no real overflow) must not suppress the
    // trailing-padding bonus for a *different*, genuinely overflowing
    // sibling — the negative-margin guard is per child and per edge, not a
    // single flag shared across the whole container.
    let (dom, _s, layout) = setup(
        "<div id=c style='width: 50px; padding: 10px'>\
         <div style='margin: 0 -5px; height: 10px'></div>\
         <div style='width: 100px; height: 10px; flex-shrink: 0'></div></div>",
    );
    let c = find_by_id(&dom, "c");
    let (width, _) = layout.scroll_size(c).unwrap();
    // content(50) + overflow past it(50) + trailing padding-right(10) = 110,
    // plus the 10px padding-left already inside client width: 70 + 50 = 120.
    assert_eq!(width, 120.0);
}

#[test]
fn inline_span_has_per_line_rects() {
    // 12 Ahem glyphs at 10px in a 60px-wide container: "aaaaa " fills line 1,
    // the span's "bbbbb bb" wraps across lines 2 and 3.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'><div style='font-family: Ahem; font-size: 10px; \
         line-height: 10px; width: 60px'>aaaaa <span id=s>bbbbb bb</span></div></body>",
    );
    let span = find_by_id(&dom, "s");
    let rects = layout.client_rects(&dom, span);
    assert_eq!(rects.len(), 2, "span wraps over two lines: {rects:?}");
    assert_eq!(rects[0].origin.y, 10.0);
    assert_eq!(rects[0].size.height, 10.0);
    assert_eq!(rects[1].origin.y, 20.0);
    // Line 2 holds "bbbbb " = 50px + the hanging collapsed space (10px):
    // fragment advances include trailing whitespace in v1.
    assert_eq!(rects[0].origin.x, 0.0);
    assert_eq!(rects[0].size.width, 60.0);
    // Line 3 holds "bb" = 20px.
    assert_eq!(rects[1].size.width, 20.0);

    // Bounding rect is the union.
    let bounding = layout.bounding_client_rect(&dom, span).unwrap();
    assert_eq!(bounding.origin.y, 10.0);
    assert_eq!(bounding.max_y(), 30.0);
    assert_eq!(bounding.size.width, 60.0);
}

#[test]
fn client_box_reports_padding_box_and_borders() {
    let (dom, _s, layout) = setup(
        "<div id=d style='width: 100px; height: 50px; padding: 10px; \
         border: 3px solid black'></div>",
    );
    let d = find_by_id(&dom, "d");
    let client = layout.client_box(d).unwrap();
    assert_eq!(client.left, 3.0);
    assert_eq!(client.top, 3.0);
    // Padding box = content 100 + padding 20.
    assert_eq!(client.width, 120.0);
    assert_eq!(client.height, 70.0);
}

#[test]
fn document_element_client_box_is_viewport() {
    let (dom, _s, layout) = setup("<div style='height: 10px'></div>");
    let html = dom.document_element().unwrap();
    let client = layout.client_box(html).unwrap();
    assert_eq!((client.width, client.height), (800.0, 600.0));
}

#[test]
fn offset_chain_walks_positioned_ancestors() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=rel style='position: relative; margin: 20px; padding: 5px; \
         border: 2px solid black'>\
         <div id=inner style='width: 10px; height: 10px'></div></div></body>",
    );
    let rel = find_by_id(&dom, "rel");
    let inner = find_by_id(&dom, "inner");

    let offsets = layout.offset_box(&dom, inner).unwrap();
    assert_eq!(offsets.parent, Some(rel));
    // inner is at rel.border(2) + rel.padding(5) from rel's border box →
    // 5px from rel's padding edge.
    assert_eq!((offsets.left, offsets.top), (5.0, 5.0));
    assert_eq!((offsets.width, offsets.height), (10.0, 10.0));

    // rel's offsetParent is the body, and a static body reports ICB-relative
    // offsets — so both axes are just rel's border-box origin (20, 20). rel's
    // top margin collapses through the body, shifting *both* border boxes down
    // together; measuring from the body's padding edge (the letter of
    // CSSOM-View) would cancel that out to a bogus offsetTop of 0.
    let rel_offsets = layout.offset_box(&dom, rel).unwrap();
    let body = rel_offsets.parent.expect("rel has an offsetParent");
    assert!(
        dom.node(body)
            .as_element()
            .is_some_and(|el| &*el.name.local == "body")
    );
    assert_eq!((rel_offsets.left, rel_offsets.top), (20.0, 20.0));
    assert_eq!(layout.border_box(rel).unwrap().origin.y, 20.0);
}

#[test]
fn scroll_size_reports_overflow_extent() {
    let (dom, _s, layout) = setup(
        "<div id=scroller style='overflow: hidden; width: 100px; height: 100px'>\
         <div style='width: 300px; height: 40px'></div></div>",
    );
    let scroller = find_by_id(&dom, "scroller");
    let (w, h) = layout.scroll_size(scroller).unwrap();
    assert_eq!(w, 300.0);
    assert_eq!(h, 100.0, "floored by the client height");
}

#[test]
fn element_from_point_hits_topmost_positioned() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=below style='position: absolute; left: 0; top: 0; \
         width: 100px; height: 100px'></div>\
         <div id=above style='position: absolute; left: 0; top: 0; \
         width: 50px; height: 50px; z-index: 5'></div></body>",
    );
    let above = find_by_id(&dom, "above");
    let below = find_by_id(&dom, "below");

    // Inside both: the higher z-index wins.
    assert_eq!(layout.element_from_point(&dom, 25.0, 25.0), Some(above));
    // Outside `above` but inside `below`.
    assert_eq!(layout.element_from_point(&dom, 75.0, 75.0), Some(below));
    // Outside both: falls back through body/html.
    let hit = layout.element_from_point(&dom, 400.0, 400.0).unwrap();
    let tag = dom.node(hit).as_element().unwrap().name.local.to_string();
    assert!(tag == "body" || tag == "html", "hit <{tag}>");
    // Outside the viewport: nothing.
    assert_eq!(layout.element_from_point(&dom, -1.0, 10.0), None);
    assert_eq!(layout.element_from_point(&dom, 10.0, 10_000.0), None);
}

#[test]
fn elements_from_point_lists_chain_topmost_first() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'><div id=outer style='width: 200px; height: 200px'>\
         <div id=inner style='width: 100px; height: 100px'></div></div></body>",
    );
    let outer = find_by_id(&dom, "outer");
    let inner = find_by_id(&dom, "inner");
    let hits = layout.elements_from_point(&dom, 50.0, 50.0);
    let inner_pos = hits.iter().position(|&n| n == inner).unwrap();
    let outer_pos = hits.iter().position(|&n| n == outer).unwrap();
    assert!(inner_pos < outer_pos, "inner before outer: {hits:?}");
    let html = dom.document_element().unwrap();
    assert_eq!(hits.last(), Some(&html));
}

#[test]
fn hit_test_attributes_text_to_span() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'><div id=d style='font-family: Ahem; font-size: 10px; \
         line-height: 10px; width: 200px'>aa<span id=s>bb</span></div></body>",
    );
    let span = find_by_id(&dom, "s");
    let d = find_by_id(&dom, "d");
    // Point over the span's glyphs (x in 20..40).
    assert_eq!(layout.element_from_point(&dom, 25.0, 5.0), Some(span));
    // Point over the plain text: the div itself.
    assert_eq!(layout.element_from_point(&dom, 5.0, 5.0), Some(d));
}

/// Regression: when a block holds inline text *beside* a block sibling, the
/// inline runs live in an anonymous wrapper block, which has no `dom_node`. The
/// attribution guard used to compare `brush_node` against
/// `b.dom_node.unwrap_or(brush_node)`, which for an anonymous box collapses to
/// `brush_node != brush_node` — always false — so the span was never reported and
/// the hit stopped at the container. Links inside such blocks were unhittable.
#[test]
fn hit_test_attributes_text_to_span_inside_an_anonymous_block() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'><div id=d style='font-family: Ahem; font-size: 10px; \
         line-height: 10px; width: 200px'>aa<span id=s>bb</span><p id=p>x</p></div></body>",
    );
    let span = find_by_id(&dom, "s");
    let d = find_by_id(&dom, "d");

    // Point over the span's glyphs (x in 20..40) on the first line.
    assert_eq!(layout.element_from_point(&dom, 25.0, 5.0), Some(span));
    // The full stack still names the container above the span.
    let hits = layout.elements_from_point(&dom, 25.0, 5.0);
    let span_pos = hits.iter().position(|&n| n == span).expect("span in stack");
    let d_pos = hits.iter().position(|&n| n == d).expect("div in stack");
    assert!(span_pos < d_pos, "span before its container: {hits:?}");
    // The anonymous wrapper must not over-collect ancestors past the container.
    let p = find_by_id(&dom, "p");
    assert!(
        !hits.contains(&p),
        "unrelated block sibling not hit: {hits:?}"
    );

    // Plain text in the same anonymous block still attributes to the div.
    assert_eq!(layout.element_from_point(&dom, 5.0, 5.0), Some(d));
}

/// The same anonymous-block path, reached through a flex item whose inline text
/// sits beside a block child.
#[test]
fn hit_test_attributes_text_to_span_inside_a_flex_item() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'><div style='display: flex'>\
         <div id=item style='font-family: Ahem; font-size: 10px; line-height: 10px'>\
         aa<span id=s>bb</span><p>x</p></div></div></body>",
    );
    let span = find_by_id(&dom, "s");
    assert_eq!(layout.element_from_point(&dom, 25.0, 5.0), Some(span));
}

/// Overflow clips at the padding edge, so a point inside a scroll container's
/// border strip hits the container but must not descend into its children.
#[test]
fn hit_test_clips_children_at_the_padding_edge() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'><div id=outer style='width: 100px; height: 100px; \
         border: 10px solid black; overflow: hidden'>\
         <div id=inner style='width: 100px; height: 100px; margin: -10px'></div>\
         </div></body>",
    );
    let outer = find_by_id(&dom, "outer");
    let inner = find_by_id(&dom, "inner");

    // (5,5) is inside the border strip: the child is clipped away there.
    assert_eq!(layout.element_from_point(&dom, 5.0, 5.0), Some(outer));
    // (20,20) is inside the padding box: the child is reachable.
    assert_eq!(layout.element_from_point(&dom, 20.0, 20.0), Some(inner));
}

/// Regression: the root's `scrollable_overflow` is the union of all descendants
/// regardless of its own `overflow`, so `<html style="overflow:hidden">` used to
/// report a positive max scroll and `scrollTo` moved the viewport.
#[test]
fn overflow_hidden_root_does_not_scroll() {
    let (_dom, _s, mut layout) = setup(
        "<html style='overflow: hidden'><body style='margin: 0'>\
         <div style='width: 10px; height: 2000px'></div></body></html>",
    );
    let result = layout.set_viewport_scroll(0.0, 500.0);
    assert_eq!(result.y, 0.0, "a hidden-overflow root pins the document");
    assert!(!result.changed);
}

/// `overflow: scroll` on the root keeps scrolling (only `hidden`/`clip` pin it).
#[test]
fn overflow_scroll_root_still_scrolls() {
    let (_dom, _s, mut layout) = setup(
        "<html style='overflow: scroll'><body style='margin: 0'>\
         <div style='width: 10px; height: 2000px'></div></body></html>",
    );
    let result = layout.set_viewport_scroll(0.0, 500.0);
    assert_eq!(result.y, 500.0);
    assert!(result.changed);
}

#[test]
fn viewport_scroll_clamps_and_shifts() {
    let (dom, _s, mut layout) = setup(
        "<body style='margin: 0'><div id=tall style='width: 10px; height: 2000px'></div></body>",
    );
    let tall = find_by_id(&dom, "tall");

    let result = layout.set_viewport_scroll(0.0, 100.0);
    assert!(result.changed);
    assert_eq!(result.y, 100.0);
    assert_eq!(layout.border_box(tall).unwrap().origin.y, -100.0);

    // Clamp: max = 2000 - 600.
    let result = layout.set_viewport_scroll(0.0, 99_999.0);
    assert_eq!(result.y, 1400.0);

    // Horizontal: no overflow → clamps to 0.
    let result = layout.set_viewport_scroll(50.0, 0.0);
    assert_eq!(result.x, 0.0);
}

#[test]
fn used_box_values_reflect_layout() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'><div id=d style='margin: 7px; padding: 4px; \
         height: 30px'></div></body>",
    );
    let d = find_by_id(&dom, "d");
    let used = layout.used_box_values(d).unwrap();
    // Auto width resolves against the 800px viewport: 800 - 14 (margins) -
    // 8 (padding) = 778 content width.
    assert_eq!(used.width, 778.0);
    assert_eq!(used.height, 30.0);
    assert_eq!(used.margin, [7.0, 7.0, 7.0, 7.0]);
    assert_eq!(used.padding, [4.0, 4.0, 4.0, 4.0]);
}

#[test]
fn inline_span_in_anonymous_ifc_has_rects() {
    // Mixed container: the span's inline run lives in an anonymous block
    // between the container and the trailing <p>.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'><div style='font-family: Ahem; font-size: 10px; \
         line-height: 10px; width: 200px'>aa<span id=s>bbb</span><p style='height: 5px'></p>\
         </div></body>",
    );
    let span = find_by_id(&dom, "s");
    let rects = layout.client_rects(&dom, span);
    assert_eq!(rects.len(), 1, "one line fragment: {rects:?}");
    assert_eq!(rects[0].origin.x, 20.0, "after the two 'a' glyphs");
    assert_eq!(rects[0].size.width, 30.0);
    assert_eq!(rects[0].size.height, 10.0);
}

#[test]
fn elements_from_point_reports_full_stack_of_siblings() {
    // Review #4: the hit walk must not stop at the first hitting sibling —
    // the plural API reports every element under the point.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=below style='position: absolute; left: 0; top: 0; \
         width: 100px; height: 100px'></div>\
         <div id=above style='position: absolute; left: 0; top: 0; \
         width: 50px; height: 50px; z-index: 5'></div></body>",
    );
    let above = find_by_id(&dom, "above");
    let below = find_by_id(&dom, "below");
    let hits = layout.elements_from_point(&dom, 25.0, 25.0);
    let above_pos = hits.iter().position(|&n| n == above).unwrap();
    let below_pos = hits
        .iter()
        .position(|&n| n == below)
        .expect("lower sibling present in the stack");
    assert!(above_pos < below_pos, "paint order: {hits:?}");
    assert_eq!(hits.last(), Some(&dom.document_element().unwrap()));
}

// === Transforms (ADR-0026) ===
//
// Every expected number below is hand-derived from the box's untransformed
// layout, which the sibling tests above pin: `body { margin: 0 }` puts a plain
// block at (0, 0), so the transform is the only thing moving it.

#[test]
fn translate_shifts_the_bounding_rect() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; \
         transform: translate(30px, -10px)'></div></body>",
    );
    let d = find_by_id(&dom, "d");
    let rect = layout.border_box(d).unwrap();
    assert_eq!((rect.origin.x, rect.origin.y), (30.0, -10.0));
    assert_eq!((rect.size.width, rect.size.height), (100.0, 40.0));
}

#[test]
fn percentage_translate_resolves_against_the_border_box() {
    // `translate(50%, 100%)` on a 100×40 box: +50px, +40px.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; \
         transform: translate(50%, 100%)'></div></body>",
    );
    let rect = layout.border_box(find_by_id(&dom, "d")).unwrap();
    assert_eq!((rect.origin.x, rect.origin.y), (50.0, 40.0));
}

#[test]
fn scale_grows_the_rect_about_the_default_center_origin() {
    // A 100×40 box at (0, 0) scaled ×2 about its centre (50, 20) spans
    // (-50, -20)..(150, 60).
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; transform: scale(2)'></div></body>",
    );
    let rect = layout.border_box(find_by_id(&dom, "d")).unwrap();
    assert_eq!((rect.origin.x, rect.origin.y), (-50.0, -20.0));
    assert_eq!((rect.size.width, rect.size.height), (200.0, 80.0));
}

#[test]
fn transform_origin_moves_the_fixed_point() {
    // The same ×2 scale about the top-left corner keeps (0, 0) put.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; transform: scale(2); \
         transform-origin: 0 0'></div></body>",
    );
    let rect = layout.border_box(find_by_id(&dom, "d")).unwrap();
    assert_eq!((rect.origin.x, rect.origin.y), (0.0, 0.0));
    assert_eq!((rect.size.width, rect.size.height), (200.0, 80.0));
}

#[test]
fn rotation_reports_the_bounding_box_of_the_quad() {
    // A 100×40 box rotated a quarter turn about its centre (50, 20) has a
    // 40×100 bounding box centred there: (30, -30)..(70, 70).
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; \
         transform: rotate(90deg)'></div></body>",
    );
    let rect = layout.border_box(find_by_id(&dom, "d")).unwrap();
    assert!((rect.origin.x - 30.0).abs() < 0.01, "{rect:?}");
    assert!((rect.origin.y - -30.0).abs() < 0.01, "{rect:?}");
    assert!((rect.size.width - 40.0).abs() < 0.01, "{rect:?}");
    assert!((rect.size.height - 100.0).abs() < 0.01, "{rect:?}");
}

#[test]
fn individual_transform_properties_compose_in_spec_order() {
    // `translate` then `rotate` then `scale` then `transform` (CSS Transforms 2
    // §"Individual Transform Properties"). Here: scale ×2 about the centre
    // (50, 20) → (-50, -20)..(150, 60), then translate by (10, 5) — the
    // translate is applied *after* the scale in matrix order, so it is not
    // itself scaled.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; \
         translate: 10px 5px; scale: 2'></div></body>",
    );
    let rect = layout.border_box(find_by_id(&dom, "d")).unwrap();
    assert_eq!((rect.origin.x, rect.origin.y), (-40.0, -15.0));
    assert_eq!((rect.size.width, rect.size.height), (200.0, 80.0));
}

#[test]
fn nested_transforms_compose_outermost_last() {
    // Outer translates by (100, 0) and scales ×2 about its own top-left; the
    // inner box sits 10px into the outer's content and is itself translated by
    // (5, 0). Innermost first: 10 + 5 = 15 in the outer's space, doubled to 30,
    // then the outer's own translate → 130.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='width: 200px; transform: translate(100px, 0) scale(2); \
         transform-origin: 0 0; padding-left: 10px'>\
         <div id=inner style='width: 20px; height: 10px; \
         transform: translate(5px, 0)'></div></div></body>",
    );
    let rect = layout.border_box(find_by_id(&dom, "inner")).unwrap();
    assert_eq!(rect.origin.x, 130.0);
    assert_eq!(rect.origin.y, 0.0);
    // The outer's ×2 scales the inner's size too.
    assert_eq!((rect.size.width, rect.size.height), (40.0, 20.0));
}

#[test]
fn a_transformed_ancestor_moves_a_descendants_rect() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div style='transform: translateY(-100%)'>\
         <div id=inner style='width: 50px; height: 20px'></div></div></body>",
    );
    let rect = layout.border_box(find_by_id(&dom, "inner")).unwrap();
    // The panel is 20px tall (its only child), so -100% is -20px.
    assert_eq!((rect.origin.x, rect.origin.y), (0.0, -20.0));
}

#[test]
fn offset_and_client_boxes_ignore_transforms() {
    // CSSOM-View defines `offset*`/`client*` on the untransformed border and
    // padding boxes; `HTMLImageElement-x-and-y-ignore-transforms.html` in WPT
    // pins exactly this. Only `getBoundingClientRect` sees the transform.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; border: 5px solid; \
         transform: translate(30px, 70px) scale(2)'>\
         <div style='height: 300px'></div></div></body>",
    );
    let d = find_by_id(&dom, "d");
    let offset = layout.offset_box(&dom, d).unwrap();
    assert_eq!((offset.left, offset.top), (0.0, 0.0));
    assert_eq!((offset.width, offset.height), (110.0, 50.0));

    let client = layout.client_box(d).unwrap();
    assert_eq!((client.left, client.top), (5.0, 5.0));
    assert_eq!((client.width, client.height), (100.0, 40.0));

    // Scrollable overflow is untransformed too (`crate::overflow`).
    let (_, scroll_h) = layout.scroll_size(d).unwrap();
    assert_eq!(scroll_h, 300.0);

    // …while the visual rect is moved and doubled.
    let rect = layout.border_box(d).unwrap();
    assert_eq!((rect.size.width, rect.size.height), (220.0, 100.0));
}

#[test]
fn content_quads_report_corners_of_a_rotated_box() {
    // A 100×40 box rotated 90° about its centre (50, 20): the top-left corner
    // (0, 0) lands at (70, -30), and the corners keep their TL/TR/BR/BL order.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; \
         transform: rotate(90deg)'></div></body>",
    );
    let quads = layout.content_quads(&dom, find_by_id(&dom, "d"));
    assert_eq!(quads.len(), 1);
    let near = |a: f32, b: f32| (a - b).abs() < 0.01;
    let q = quads[0];
    assert!(near(q[0].x, 70.0) && near(q[0].y, -30.0), "{q:?}");
    assert!(near(q[1].x, 70.0) && near(q[1].y, 70.0), "{q:?}");
    assert!(near(q[2].x, 30.0) && near(q[2].y, 70.0), "{q:?}");
    assert!(near(q[3].x, 30.0) && near(q[3].y, -30.0), "{q:?}");
}

#[test]
fn content_quads_report_one_quad_per_line() {
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'><div style='font-family: Ahem; font-size: 10px; \
         line-height: 10px; width: 30px'><span id=s>aaa aaa</span></div></body>",
    );
    let quads = layout.content_quads(&dom, find_by_id(&dom, "s"));
    assert_eq!(quads.len(), 2, "wraps onto two lines: {quads:?}");
    assert_eq!(quads[0][0], oxidepage_base::Point::new(0.0, 0.0));
    assert_eq!(quads[1][0], oxidepage_base::Point::new(0.0, 10.0));
}

#[test]
fn hit_testing_inverts_the_transform() {
    // The panel is translated 200px right; a probe at its *painted* position
    // hits it and a probe at its untransformed one does not.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; \
         transform: translateX(200px)'></div></body>",
    );
    let d = find_by_id(&dom, "d");
    assert_eq!(layout.element_from_point(&dom, 250.0, 20.0), Some(d));
    let miss = layout.element_from_point(&dom, 50.0, 20.0).unwrap();
    assert_ne!(miss, d, "the untransformed position must not hit");
}

#[test]
fn hit_testing_inside_and_outside_a_rotated_box() {
    // A 100×40 box rotated 90° about its centre occupies (30, -30)..(70, 70).
    // (35, 60) is inside that quad; (10, 30) is inside the *untransformed* box
    // but outside the rotated one.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; \
         transform: rotate(90deg)'></div></body>",
    );
    let d = find_by_id(&dom, "d");
    assert_eq!(layout.element_from_point(&dom, 35.0, 60.0), Some(d));
    assert_ne!(layout.element_from_point(&dom, 10.0, 30.0), Some(d));
}

#[test]
fn a_zero_scaled_box_is_not_hit_testable() {
    // `scale(0)` is singular: there is no inverse, and the box collapses to a
    // point that no probe can land on.
    let (dom, _s, layout) = setup(
        "<body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; transform: scale(0)'></div></body>",
    );
    let d = find_by_id(&dom, "d");
    let hits = layout.elements_from_point(&dom, 50.0, 20.0);
    assert!(!hits.contains(&d), "{hits:?}");
}
