//! A `min-content`/`max-content`-resolved size must not ratchet: taffy clamps
//! a computed size against a box's own `min_size`/`max_size` regardless of
//! sizing mode, so re-measuring on a later reflow without first resetting the
//! previously-resolved field to `AUTO` would clamp the new (possibly
//! smaller) measurement back up to the old one.

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

fn set_style_attr(dom: &mut DomTree, node: NodeId, value: &str) {
    dom.set_attribute(
        node,
        oxidepage_dom::node::attr_name(html5ever::local_name!("style")),
        value.into(),
    );
}

#[test]
fn min_width_min_content_shrinks_back_down() {
    let (mut dom, mut style, mut layout) = setup(
        "<body style='margin: 0'>\
         <div id=p style='display: flex; width: 10px; min-width: min-content; flex-wrap: wrap'>\
         <div id=c style='width: 100px; height: 20px'></div>\
         </div></body>",
    );
    let p = find_by_id(&dom, "p");
    let c = find_by_id(&dom, "c");
    let p_box = layout.tree().box_for_node(p).unwrap();
    assert_eq!(layout.tree().box_(p_box).final_layout.size.width, 100.0);

    set_style_attr(&mut dom, c, "width: 20px; height: 20px; flex-shrink: 0");
    style.resolve_styles(&mut dom);
    layout.reflow(&mut dom, &mut style);

    let p_box = layout.tree().box_for_node(p).unwrap();
    assert_eq!(
        layout.tree().box_(p_box).final_layout.size.width,
        20.0,
        "min-width: min-content must re-measure down, not ratchet at the old value"
    );
}
