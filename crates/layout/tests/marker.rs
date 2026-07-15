//! List markers (`crates/layout/src/marker.rs`): marker content
//! (`list-style-type` + structural numbering), placement
//! (`list-style-position`), and the invariants an outside marker must keep —
//! it must not disturb the item's size, its scrollable overflow, or its
//! `getBoundingClientRect`, but a point on it must still hit the item.
//!
//! Marker *strings* are font-independent, so they are asserted exactly.
//! Positions are asserted relationally (the marker's right edge sits a gap
//! before the item's content edge), which holds for any font.

use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};
use oxidepage_layout::{BoxId, LayoutEngine, PseudoBox};
use oxidepage_style::{StyleEngine, Viewport};

/// `list-style-position: outside` gap, in em (mirrors `marker::MARKER_GAP_EM`).
const GAP_EM: f32 = 0.5;

fn setup(html: &str) -> (DomTree, LayoutEngine) {
    let mut dom = parse_document(html, ParseOptions::default()).tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut layout = LayoutEngine::new(Viewport::default());
    layout.reflow(&mut dom, &mut style);
    (dom, layout)
}

fn find_by_id(tree: &DomTree, id_attr: &str) -> NodeId {
    tree.inclusive_descendants(tree.document())
        .find(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| el.id().map(|a| &**a) == Some(id_attr))
        })
        .unwrap_or_else(|| panic!("no element with id={id_attr}"))
}

fn list_items(dom: &DomTree) -> Vec<NodeId> {
    dom.inclusive_descendants(dom.document())
        .filter(|&id| {
            dom.node(id)
                .as_element()
                .is_some_and(|el| &*el.name.local == "li")
        })
        .collect()
}

/// The marker box of `node`'s principal box, if it generated one.
fn marker_box(layout: &LayoutEngine, node: NodeId) -> Option<BoxId> {
    let tree = layout.tree();
    let item = tree.box_for_node(node)?;
    tree.box_(item)
        .children
        .iter()
        .copied()
        .find(|&child| tree.box_(child).pseudo == Some(PseudoBox::Marker))
}

/// The text an outside marker renders.
fn marker_string(layout: &LayoutEngine, node: NodeId) -> Option<String> {
    let id = marker_box(layout, node)?;
    Some(layout.tree().box_(id).ifc.as_ref()?.text.clone())
}

/// Every `<li>`'s marker string, in document order.
fn marker_strings(dom: &DomTree, layout: &LayoutEngine) -> Vec<String> {
    list_items(dom)
        .into_iter()
        .map(|li| marker_string(layout, li).unwrap_or_default())
        .collect()
}

/// The text of the IFC an element's own principal box owns (an *inside* marker
/// is part of it, an outside one is not).
fn item_text(layout: &LayoutEngine, node: NodeId) -> String {
    let tree = layout.tree();
    let item = tree.box_for_node(node).expect("no box");
    tree.box_(item)
        .ifc
        .as_ref()
        .map(|ifc| ifc.text.clone())
        .unwrap_or_default()
}

// === Content: list-style-type ===

#[test]
fn ul_and_ol_get_their_default_markers() {
    let (dom, layout) = setup("<ul><li id=a>alpha</li></ul><ol><li id=b>one</li></ol>");
    assert_eq!(
        marker_string(&layout, find_by_id(&dom, "a")).as_deref(),
        Some("\u{2022}")
    );
    assert_eq!(
        marker_string(&layout, find_by_id(&dom, "b")).as_deref(),
        Some("1.")
    );
}

#[test]
fn nested_uls_cycle_disc_circle_square() {
    let (dom, layout) = setup(
        "<ul><li id=l1>1<ul><li id=l2>2<ul><li id=l3>3<ul><li id=l4>4</li></ul></li></ul></li></ul>",
    );
    let of = |id| marker_string(&layout, find_by_id(&dom, id)).unwrap();
    assert_eq!(of("l1"), "\u{2022}", "disc");
    assert_eq!(of("l2"), "\u{25e6}", "circle");
    assert_eq!(of("l3"), "\u{25aa}", "square");
    // Deeper than the UA cascade goes: the innermost rule keeps applying.
    assert_eq!(of("l4"), "\u{25aa}", "square");
}

#[test]
fn list_style_type_none_generates_no_marker() {
    let (dom, layout) = setup("<ul style='list-style-type: none'><li id=a>alpha</li></ul>");
    let a = find_by_id(&dom, "a");
    assert!(marker_box(&layout, a).is_none());
    assert_eq!(item_text(&layout, a), "alpha");
}

#[test]
fn counter_styles_render_their_representations() {
    let case = |list_style: &str| {
        let html = format!(
            "<ol style='list-style-type: {list_style}'>\
             <li></li><li></li><li></li><li></li><li></li></ol>"
        );
        let (dom, layout) = setup(&html);
        marker_strings(&dom, &layout)
    };
    assert_eq!(case("decimal"), ["1.", "2.", "3.", "4.", "5."]);
    assert_eq!(
        case("decimal-leading-zero"),
        ["01.", "02.", "03.", "04.", "05."]
    );
    assert_eq!(case("lower-alpha"), ["a.", "b.", "c.", "d.", "e."]);
    assert_eq!(case("upper-alpha"), ["A.", "B.", "C.", "D.", "E."]);
    assert_eq!(case("lower-roman"), ["i.", "ii.", "iii.", "iv.", "v."]);
    assert_eq!(case("upper-roman"), ["I.", "II.", "III.", "IV.", "V."]);
    assert_eq!(case("square"), ["\u{25aa}"; 5]);
    // An unknown / unimplemented counter style falls back to decimal.
    assert_eq!(case("hebrew"), ["1.", "2.", "3.", "4.", "5."]);
    // A `<string>` list-style-type is the marker verbatim, with no suffix.
    assert_eq!(case("\"->\""), ["->"; 5]);
}

// === Content: structural numbering ===

#[test]
fn ol_start_offsets_the_first_ordinal() {
    let (dom, layout) = setup("<ol start=8><li></li><li></li></ol>");
    assert_eq!(marker_strings(&dom, &layout), ["8.", "9."]);
}

#[test]
fn li_value_resets_the_running_counter() {
    let (dom, layout) = setup("<ol><li></li><li value=20></li><li></li></ol>");
    assert_eq!(marker_strings(&dom, &layout), ["1.", "20.", "21."]);
}

#[test]
fn ol_reversed_counts_down_from_the_item_count() {
    let (dom, layout) = setup("<ol reversed><li></li><li></li><li></li></ol>");
    assert_eq!(marker_strings(&dom, &layout), ["3.", "2.", "1."]);

    // `start` wins over the item count, and the direction still reverses.
    let (dom, layout) = setup("<ol reversed start=5><li></li><li></li></ol>");
    assert_eq!(marker_strings(&dom, &layout), ["5.", "4."]);
}

#[test]
fn nested_ol_restarts_numbering() {
    let (dom, layout) =
        setup("<ol><li id=a>a<ol><li id=b>b</li><li id=c>c</li></ol></li><li id=d>d</li></ol>");
    let of = |id| marker_string(&layout, find_by_id(&dom, id)).unwrap();
    assert_eq!(of("a"), "1.");
    assert_eq!(of("b"), "1.", "the nested list restarts");
    assert_eq!(of("c"), "2.");
    assert_eq!(of("d"), "2.", "the outer list is unaffected by the nesting");
}

// === Placement ===

#[test]
fn inside_marker_is_the_items_first_inline_content() {
    let (dom, layout) = setup(
        "<ul style='list-style-position: inside'><li id=a>alpha</li></ul>\
         <ol style='list-style-position: inside'><li id=b>one</li></ol>",
    );
    let a = find_by_id(&dom, "a");
    let b = find_by_id(&dom, "b");
    // No box of its own: the marker is text in the item's own IFC, separated
    // from the item's content by the counter style's suffix space.
    assert!(marker_box(&layout, a).is_none());
    assert!(marker_box(&layout, b).is_none());
    assert_eq!(item_text(&layout, a), "\u{2022} alpha");
    assert_eq!(item_text(&layout, b), "1. one");
}

#[test]
fn outside_marker_sits_a_gap_before_the_items_content_edge() {
    let (dom, layout) = setup(
        "<body style='margin: 0'>\
         <ul style='margin: 0; padding-left: 40px'>\
         <li id=a style='font-size: 16px; padding-left: 5px; border-left: 3px solid'>alpha</li>\
         </ul></body>",
    );
    let a = find_by_id(&dom, "a");
    let item_rect = layout.border_box(a).expect("item box");
    // The item's content edge: border-box left + border-left + padding-left.
    let content_left = item_rect.origin.x + 3.0 + 5.0;

    let marker = marker_box(&layout, a).expect("outside marker box");
    let tree = layout.tree();
    let m = tree.box_(marker);
    assert!(m.is_outside_marker());

    // Locations are relative to the item's border box.
    let marker_left = item_rect.origin.x + m.final_layout.location.x;
    let marker_right = marker_left + m.final_layout.size.width;
    let gap = GAP_EM * m.font_size;
    // Rounding lands the marker on a whole pixel, so allow one.
    assert!(
        (marker_right - (content_left - gap)).abs() <= 1.0,
        "marker right {marker_right} should be {gap} before content edge {content_left}",
    );
    assert!(
        marker_right < content_left,
        "the marker is outside the content"
    );
    // Top-aligned with the item's content box (which has no top border or
    // padding here), so the marker shares the item's first baseline.
    assert_eq!(m.final_layout.location.y, 0.0);
}

#[test]
fn numbers_right_align_across_a_digit_boundary() {
    let (dom, layout) = setup("<ol start=9><li id=a></li><li id=b></li></ol>");
    let tree = layout.tree();
    let right_edge = |id| {
        let li = find_by_id(&dom, id);
        let item = layout.border_box(li).unwrap();
        let m = tree.box_(marker_box(&layout, li).unwrap());
        item.origin.x + m.final_layout.location.x + m.final_layout.size.width
    };
    assert_eq!(marker_strings(&dom, &layout), ["9.", "10."]);
    // "9." and "10." differ in width, but both end at the same x — which is
    // what makes a numbered list line up under its periods.
    assert!(
        (right_edge("a") - right_edge("b")).abs() <= 1.0,
        "markers should be right-aligned: {} vs {}",
        right_edge("a"),
        right_edge("b"),
    );
}

#[test]
fn outside_marker_does_not_resize_the_item_or_its_overflow() {
    let (dom, layout) = setup(
        "<body style='margin: 0'><ul style='margin: 0; padding-left: 40px'>\
         <li id=a>alpha</li></ul></body>",
    );
    let (dom_plain, plain) = setup(
        "<body style='margin: 0'><ul style='margin: 0; padding-left: 40px; list-style-type: none'>\
         <li id=a>alpha</li></ul></body>",
    );
    let a = find_by_id(&dom, "a");
    let a_plain = find_by_id(&dom_plain, "a");
    assert_eq!(
        layout.border_box(a).unwrap(),
        plain.border_box(a_plain).unwrap(),
        "the marker must not change the item's border box",
    );

    // The marker hangs off the item's start edge; CSS clips the scrollable
    // overflow region there, so it must not widen `scrollWidth`.
    let item = layout.tree().box_for_node(a).unwrap();
    let overflow = layout.tree().box_(item).scrollable_overflow;
    assert!(
        overflow.origin.x >= 0.0,
        "outside marker leaked into the item's scrollable overflow: {overflow:?}",
    );
}

// === Hit testing ===

#[test]
fn a_point_on_an_outside_marker_hits_the_list_item() {
    let (dom, layout) = setup(
        "<body style='margin: 0'><ul style='margin: 0; padding-left: 40px'>\
         <li id=a>alpha</li></ul></body>",
    );
    let a = find_by_id(&dom, "a");
    let item = layout.border_box(a).unwrap();
    let tree = layout.tree();
    let m = tree.box_(marker_box(&layout, a).unwrap());
    let x = item.origin.x + m.final_layout.location.x + m.final_layout.size.width / 2.0;
    let y = item.origin.y + m.final_layout.location.y + m.final_layout.size.height / 2.0;

    assert!(
        x < item.origin.x,
        "the probe point is outside the item's box"
    );
    assert_eq!(
        layout.element_from_point(&dom, x, y),
        Some(a),
        "a point on the marker must resolve to the <li>",
    );
    // …and the item is reported once, not twice.
    let stack = layout.elements_from_point(&dom, x, y);
    assert_eq!(stack.iter().filter(|&&n| n == a).count(), 1, "{stack:?}");
}

// === Incremental reflow ===

#[test]
fn changing_list_style_rebuilds_the_marker() {
    // The marker's content, its position (inline vs. its own box) and its very
    // existence are all captured at box-construction time, so a `list-style-*`
    // change has to leave the incremental-patch path — otherwise the item keeps
    // painting its old bullet.
    let mut dom = parse_document(
        "<ul id=list><li id=a>alpha</li></ul>",
        ParseOptions::default(),
    )
    .tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut layout = LayoutEngine::new(Viewport::default());
    layout.reflow(&mut dom, &mut style);

    let a = find_by_id(&dom, "a");
    let list = find_by_id(&dom, "list");
    assert_eq!(marker_string(&layout, a).as_deref(), Some("\u{2022}"));
    assert_eq!(layout.reflow_counts(), (1, 0));

    let set_style = |dom: &mut DomTree, value: &str| {
        dom.set_attribute(
            list,
            oxidepage_dom::node::attr_name(html5ever::local_name!("style")),
            value.into(),
        );
    };

    // `list-style-type` inherits onto the item: its marker text changes.
    set_style(&mut dom, "list-style-type: upper-roman");
    layout.reflow(&mut dom, &mut style);
    assert_eq!(layout.reflow_counts().0, 2, "must rebuild, not patch");
    assert_eq!(marker_string(&layout, a).as_deref(), Some("I."));

    // `list-style-position` moves the marker from its own box into the item.
    set_style(&mut dom, "list-style-position: inside");
    layout.reflow(&mut dom, &mut style);
    assert!(marker_box(&layout, a).is_none());
    assert_eq!(item_text(&layout, a), "\u{2022} alpha");

    // …and `none` removes it entirely.
    set_style(&mut dom, "list-style-type: none");
    layout.reflow(&mut dom, &mut style);
    assert!(marker_box(&layout, a).is_none());
    assert_eq!(item_text(&layout, a), "alpha");
}

// === Container kinds ===

#[test]
fn list_items_in_other_container_kinds_still_get_markers() {
    // The marker box is `position: absolute` and placed by hand, so it works the
    // same in a block item, an inline (IFC-root) item, and a flex/grid item.
    let (dom, layout) = setup(
        "<ul>\
         <li id=inline>plain text (an IFC root)</li>\
         <li id=block><p>a block child</p></li>\
         <li id=flex style='display: flex list-item'><span>flex</span></li>\
         <li id=grid style='display: grid list-item'><span>grid</span></li>\
         <li id=empty></li>\
         </ul>",
    );
    for id in ["inline", "block", "flex", "grid", "empty"] {
        let li = find_by_id(&dom, id);
        let marker = marker_box(&layout, li)
            .unwrap_or_else(|| panic!("<li id={id}> generated no marker box"));
        let m = layout.tree().box_(marker);
        assert!(m.is_outside_marker(), "{id}");
        assert!(
            m.final_layout.size.width > 0.0,
            "<li id={id}>'s marker was never measured",
        );
        assert!(
            m.final_layout.location.x < 0.0,
            "<li id={id}>'s marker should hang off the item's start edge",
        );
    }
}
