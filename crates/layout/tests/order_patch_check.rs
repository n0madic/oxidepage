//! `order` must force a full rebuild on the incremental-patch path: taffy has
//! no `order` field of its own (only `construct::collect_flex_grid_children`
//! pre-sorts `children` by it, on a rebuild), so a patch that merely
//! refreshed `LayoutBox::order` without re-sorting would silently leave
//! reordered items in their old positions.

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

fn set_style_attr(dom: &mut DomTree, node: NodeId, value: &str) {
    dom.set_attribute(
        node,
        oxidepage_dom::node::attr_name(html5ever::local_name!("style")),
        value.into(),
    );
}

#[test]
fn order_change_forces_rebuild_and_resorts_children() {
    let (mut dom, mut style, mut layout) = setup(
        "<body style='margin: 0'><div style='display: flex; width: 100px'>\
         <div id=x style='order: 1; width: 20px; height: 20px'></div>\
         <div id=y style='order: 2; width: 20px; height: 20px'></div>\
         </div></body>",
    );
    let x = find_by_id(&dom, "x");
    let y = find_by_id(&dom, "y");

    set_style_attr(&mut dom, x, "order: 3; width: 20px; height: 20px");
    style.resolve_styles(&mut dom);
    layout
        .reflow(&mut dom, &mut style)
        .expect("layout completes");

    assert_eq!(
        layout.reflow_counts().0,
        2,
        "an order change is out of a patch's reach and must rebuild"
    );
    let x_box = layout.tree().box_for_node(x).unwrap();
    let y_box = layout.tree().box_for_node(y).unwrap();
    assert_eq!(
        layout.tree().box_(y_box).final_layout.location.x,
        0.0,
        "y (order 2) is now first"
    );
    assert_eq!(
        layout.tree().box_(x_box).final_layout.location.x,
        20.0,
        "x (order 3) is now second"
    );
}
