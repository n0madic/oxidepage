//! WP-D: end-to-end reflow tests. All metric-dependent assertions use the
//! bundled Ahem font (every glyph is a 1em × 1em square), so results are
//! identical across platforms.

use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};
use oxidepage_layout::LayoutEngine;
use oxidepage_layout::tree::BoxId;
use oxidepage_style::{StyleEngine, Viewport};

fn find_element(tree: &DomTree, local: &str) -> NodeId {
    tree.inclusive_descendants(tree.document())
        .find(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| &*el.name.local == local)
        })
        .unwrap_or_else(|| panic!("no <{local}> in document"))
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

fn setup(html: &str) -> (DomTree, StyleEngine, LayoutEngine) {
    let mut dom = parse_document(html, ParseOptions::default()).tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut layout = LayoutEngine::new(Viewport::default());
    layout.reflow(&mut dom, &mut style);
    (dom, style, layout)
}

fn box_of(layout: &LayoutEngine, dom: &DomTree, id_attr: &str) -> BoxId {
    layout
        .tree()
        .box_for_node(find_by_id(dom, id_attr))
        .unwrap_or_else(|| panic!("no box for #{id_attr}"))
}

#[test]
fn block_sizes_fill_viewport_width() {
    let (dom, _style, layout) = setup("<div id=d style='height: 50px'></div>");
    let body = layout
        .tree()
        .box_for_node(find_element(&dom, "body"))
        .unwrap();
    // Default UA body margin is 8px; viewport is 800px wide.
    assert_eq!(layout.tree().box_(body).final_layout.size.width, 784.0);

    let d = box_of(&layout, &dom, "d");
    let d_layout = layout.tree().box_(d).final_layout;
    assert_eq!(d_layout.size.width, 784.0);
    assert_eq!(d_layout.size.height, 50.0);
}

#[test]
fn fixed_size_blocks_stack_vertically() {
    let (dom, _style, layout) = setup(
        "<div id=a style='width: 100px; height: 30px'></div>\
         <div id=b style='width: 200px; height: 40px'></div>",
    );
    let a = layout.tree().box_(box_of(&layout, &dom, "a")).final_layout;
    let b = layout.tree().box_(box_of(&layout, &dom, "b")).final_layout;
    assert_eq!((a.size.width, a.size.height), (100.0, 30.0));
    assert_eq!((b.size.width, b.size.height), (200.0, 40.0));
    assert_eq!(b.location.y, a.location.y + 30.0);
    assert_eq!(a.location.x, b.location.x);
}

#[test]
fn floated_blocks_share_a_line() {
    let (dom, _style, layout) = setup(
        "<body style='margin: 0'>\
         <div id=container style='width: 300px'>\
           <div id=a style='float: left; width: 100px; height: 30px'></div>\
           <div id=b style='float: left; width: 200px; height: 40px'></div>\
         </div></body>",
    );
    let a = layout.tree().box_(box_of(&layout, &dom, "a")).final_layout;
    let b = layout.tree().box_(box_of(&layout, &dom, "b")).final_layout;
    let container = layout
        .tree()
        .box_(box_of(&layout, &dom, "container"))
        .final_layout;

    assert_eq!((a.location.x, a.location.y), (0.0, 0.0));
    assert_eq!((b.location.x, b.location.y), (100.0, 0.0));
    assert_eq!(container.size.height, 0.0, "floats are out of flow");
}

#[test]
fn fractional_percentage_floats_that_fill_container_share_a_line() {
    let (dom, _style, layout) = setup(
        "<body style='margin: 0'>\
         <div style='width: 1258px'>\
           <div id=a style='float: left; width: 16.66666667%; height: 10px'></div>\
           <div id=b style='float: left; width: 66.66666667%; height: 10px'></div>\
           <div id=c style='float: left; width: 16.66666667%; height: 10px'></div>\
         </div></body>",
    );
    let a = layout.tree().box_(box_of(&layout, &dom, "a")).final_layout;
    let b = layout.tree().box_(box_of(&layout, &dom, "b")).final_layout;
    let c = layout.tree().box_(box_of(&layout, &dom, "c")).final_layout;

    assert_eq!((a.location.y, b.location.y, c.location.y), (0.0, 0.0, 0.0));
    assert_eq!(c.location.x, 1048.0);
}

#[test]
fn clear_both_moves_past_a_float() {
    let (dom, _style, layout) = setup(
        "<body style='margin: 0'>\
         <div id=container style='width: 300px'>\
           <div style='float: left; width: 100px; height: 40px'></div>\
           <div id=clear style='clear: both; height: 0'></div>\
         </div></body>",
    );
    let clear = layout
        .tree()
        .box_(box_of(&layout, &dom, "clear"))
        .final_layout;
    let container = layout
        .tree()
        .box_(box_of(&layout, &dom, "container"))
        .final_layout;

    assert_eq!(clear.location.y, 40.0);
    assert_eq!(container.size.height, 40.0);
}

#[test]
fn clearfix_after_pseudo_contains_a_float() {
    let mut dom = parse_document(
        "<style>#container::after { content: ''; display: table; clear: both }</style>\
         <body style='margin: 0'>\
         <div id=container style='width: 300px'>\
           <div style='float: left; width: 100px; height: 40px'></div>\
         </div></body>",
        ParseOptions::default(),
    )
    .tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let style_el = find_element(&dom, "style");
    let css = dom.text_content(style_el);
    let sheet = style.make_stylesheet(&css, dom.url_extra_data());
    style.add_sheet_for_node(&dom, style_el, sheet);
    let mut layout = LayoutEngine::new(Viewport::default());
    layout.reflow(&mut dom, &mut style);

    let container_box = box_of(&layout, &dom, "container");
    let pseudo = layout
        .tree()
        .box_(container_box)
        .children
        .iter()
        .map(|&id| layout.tree().box_(id))
        .find(|child| child.pseudo.is_some())
        .expect("clearfix ::after box");
    assert_eq!(pseudo.style.clear, taffy::Clear::Both);
    assert_eq!(pseudo.style.display, taffy::Display::Block);
    assert!(!pseudo.style.item_is_table);
    let container = layout.tree().box_(container_box).final_layout;

    assert_eq!(container.size.height, 40.0);
}

#[test]
fn bootstrap_clearfix_preserves_float_order_and_height() {
    let mut dom = parse_document(
        "<style>\
           #row::before, #row::after { content: ' '; display: table }\
           #row::after { clear: both }\
         </style>\
         <body style='margin: 0'>\
         <div id=row style='width: 1258px'>\
           <div id=a style='position: relative; float: left; width: 16.66666667%; height: 150px'></div>\
           <div id=b style='position: relative; float: left; width: 83.33333333%; height: 32px'></div>\
         </div></body>",
        ParseOptions::default(),
    )
    .tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let style_el = find_element(&dom, "style");
    let css = dom.text_content(style_el);
    let sheet = style.make_stylesheet(&css, dom.url_extra_data());
    style.add_sheet_for_node(&dom, style_el, sheet);
    let mut layout = LayoutEngine::new(Viewport::default());
    layout.reflow(&mut dom, &mut style);

    let a = layout.tree().box_(box_of(&layout, &dom, "a")).final_layout;
    let b = layout.tree().box_(box_of(&layout, &dom, "b")).final_layout;
    let row = layout
        .tree()
        .box_(box_of(&layout, &dom, "row"))
        .final_layout;

    assert_eq!((a.location.x, a.location.y), (0.0, 0.0));
    assert_eq!((b.location.x, b.location.y), (210.0, 0.0));
    assert_eq!(row.size.height, 150.0);
}

#[test]
fn clearfix_bfc_includes_bottom_padding_after_floats() {
    let mut dom = parse_document(
        "<style>#row::after { content: ''; display: table; clear: both }</style>\
         <body style='margin: 0'>\
         <div id=row style='width: 300px; padding-bottom: 30px'>\
           <div style='float: left; width: 100px; height: 150px'></div>\
           <div style='float: left; width: 100px; height: 32px'></div>\
         </div></body>",
        ParseOptions::default(),
    )
    .tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let style_el = find_element(&dom, "style");
    let css = dom.text_content(style_el);
    let sheet = style.make_stylesheet(&css, dom.url_extra_data());
    style.add_sheet_for_node(&dom, style_el, sheet);
    let mut layout = LayoutEngine::new(Viewport::default());
    layout.reflow(&mut dom, &mut style);

    let row = layout
        .tree()
        .box_(box_of(&layout, &dom, "row"))
        .final_layout;
    assert_eq!(row.size.height, 180.0, "150px float + 30px padding");
}

#[test]
fn relative_offset_moves_a_float_without_affecting_flow_height() {
    let mut dom = parse_document(
        "<style>#row::after { content: ''; display: table; clear: both }</style>\
         <body style='margin: 0'>\
         <div id=row style='width: 300px'>\
           <div id=f style='position: relative; top: 30px; float: left; \
                width: 100px; height: 40px'></div>\
         </div></body>",
        ParseOptions::default(),
    )
    .tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let style_el = find_element(&dom, "style");
    let css = dom.text_content(style_el);
    let sheet = style.make_stylesheet(&css, dom.url_extra_data());
    style.add_sheet_for_node(&dom, style_el, sheet);
    let mut layout = LayoutEngine::new(Viewport::default());
    layout.reflow(&mut dom, &mut style);

    let float = layout.tree().box_(box_of(&layout, &dom, "f")).final_layout;
    let row = layout
        .tree()
        .box_(box_of(&layout, &dom, "row"))
        .final_layout;
    assert_eq!(float.location.y, 30.0);
    assert_eq!(row.size.height, 40.0, "relative offset is out of flow");
}

#[test]
fn ahem_text_measures_exactly() {
    // 5 Ahem glyphs at 20px = 100px wide; line height = 20px (Ahem's
    // ascent+descent = 1em, line-height: normal resolves to 1.2em → but the
    // box is sized by the line box, so use an explicit line-height).
    let (dom, _style, layout) = setup(
        "<div id=d style='font-family: Ahem; font-size: 20px; line-height: 20px; \
         width: 200px'>XXXXX</div>",
    );
    let d = layout.tree().box_(box_of(&layout, &dom, "d")).final_layout;
    assert_eq!(d.size.width, 200.0);
    assert_eq!(d.size.height, 20.0);
}

#[test]
fn ahem_text_wraps_into_lines() {
    // 10 glyphs at 20px in a 100px container → 2 lines of 5 → height 40px.
    let (dom, _style, layout) = setup(
        "<div id=d style='font-family: Ahem; font-size: 20px; line-height: 20px; \
         width: 100px'>XXXXX XXXX</div>",
    );
    let d = layout.tree().box_(box_of(&layout, &dom, "d")).final_layout;
    assert_eq!(d.size.height, 40.0);
}

#[test]
fn shrink_to_fit_inline_content() {
    // Block with no width in a max-content context: float-free proxy is an
    // inline-block, which sizes to its content: 3 glyphs * 16px = 48px.
    let (dom, _style, layout) = setup(
        "<span id=s style='display: inline-block; font-family: Ahem; \
         font-size: 16px; line-height: 16px'>abc</span>",
    );
    let s = layout.tree().box_(box_of(&layout, &dom, "s")).final_layout;
    assert_eq!((s.size.width, s.size.height), (48.0, 16.0));
}

#[test]
fn flex_row_distributes_space() {
    let (dom, _style, layout) = setup(
        "<div id=f style='display: flex; width: 300px; height: 50px'>\
         <div id=a style='flex: 1'></div><div id=b style='flex: 2'></div></div>",
    );
    let a = layout.tree().box_(box_of(&layout, &dom, "a")).final_layout;
    let b = layout.tree().box_(box_of(&layout, &dom, "b")).final_layout;
    assert_eq!(a.size.width, 100.0);
    assert_eq!(b.size.width, 200.0);
    assert_eq!(a.size.height, 50.0);
    assert_eq!(b.location.x, 100.0);
}

#[test]
fn grid_two_columns() {
    let (dom, _style, layout) = setup(
        "<div style='display: grid; grid-template-columns: 100px 150px; width: 300px'>\
         <div id=a style='height: 20px'></div><div id=b style='height: 20px'></div></div>",
    );
    let a = layout.tree().box_(box_of(&layout, &dom, "a")).final_layout;
    let b = layout.tree().box_(box_of(&layout, &dom, "b")).final_layout;
    assert_eq!(a.size.width, 100.0);
    assert_eq!(b.size.width, 150.0);
    assert_eq!(b.location.x, 100.0);
}

#[test]
fn padding_and_border_offset_content() {
    let (dom, _style, layout) = setup(
        "<div style='padding: 10px; border: 5px solid black; width: 100px'>\
         <div id=inner style='height: 20px'></div></div>",
    );
    let inner = layout
        .tree()
        .box_(box_of(&layout, &dom, "inner"))
        .final_layout;
    assert_eq!(inner.location.x, 15.0);
    assert_eq!(inner.location.y, 15.0);
    assert_eq!(inner.size.width, 100.0);
}

#[test]
fn atomic_inline_block_participates_in_line_layout() {
    let (dom, _style, layout) = setup(
        "<div id=d style='font-family: Ahem; font-size: 10px; line-height: 10px; width: 200px'>\
         XX<span id=s style='display: inline-block; width: 30px; height: 30px'></span>XX</div>",
    );
    let s = layout.tree().box_(box_of(&layout, &dom, "s")).final_layout;
    assert_eq!((s.size.width, s.size.height), (30.0, 30.0));
    // Placed after the first two 10px glyphs.
    assert_eq!(s.location.x, 20.0);
    // Line height grows to fit the 30px inline block.
    let d = layout.tree().box_(box_of(&layout, &dom, "d")).final_layout;
    assert_eq!(d.size.height, 30.0);
}

#[test]
fn absolute_child_does_not_affect_parent_height() {
    let (dom, _style, layout) = setup(
        "<div id=rel style='position: relative; height: 40px'>\
         <div id=abs style='position: absolute; top: 10px; left: 20px; \
         width: 50px; height: 60px'></div></div>",
    );
    let rel = layout
        .tree()
        .box_(box_of(&layout, &dom, "rel"))
        .final_layout;
    let abs = layout
        .tree()
        .box_(box_of(&layout, &dom, "abs"))
        .final_layout;
    assert_eq!(rel.size.height, 40.0);
    assert_eq!((abs.location.x, abs.location.y), (20.0, 10.0));
    assert_eq!((abs.size.width, abs.size.height), (50.0, 60.0));
}

#[test]
fn absolute_box_with_opposing_insets_and_auto_margins_is_centered() {
    let (dom, _style, layout) = setup(
        "<body style='margin: 0'><div style='position: relative; width: 211px; height: 100px'>\
         <div id=abs style='position: absolute; left: 0; right: 0; top: 0; bottom: 0; \
         width: 155px; height: 40px; margin: auto'></div></div></body>",
    );
    let abs = layout
        .tree()
        .box_(box_of(&layout, &dom, "abs"))
        .final_layout;

    assert_eq!((abs.location.x, abs.location.y), (28.0, 30.0));
    assert_eq!((abs.size.width, abs.size.height), (155.0, 40.0));
}

#[test]
fn post_layout_offsets_do_not_accumulate_on_incremental_reflow() {
    let (mut dom, mut style, mut layout) = setup(
        "<body style='margin: 0'>\
         <div style='position: relative; width: 211px; height: 100px'>\
           <div id=abs style='position: absolute; left: 0; right: 0; width: 155px; \
                height: 40px; margin-left: auto; margin-right: auto'></div>\
         </div>\
         <div id=float style='position: relative; top: 30px; float: left; \
              width: 20px; height: 20px'></div>\
         <div id=trigger style='width: 10px; height: 1px'></div>\
         </body>",
    );
    let abs = find_by_id(&dom, "abs");
    let float = find_by_id(&dom, "float");
    let trigger = find_by_id(&dom, "trigger");
    let initial_abs = layout
        .tree()
        .box_(layout.tree().box_for_node(abs).unwrap())
        .final_layout;
    let initial_float = layout
        .tree()
        .box_(layout.tree().box_for_node(float).unwrap())
        .final_layout;
    assert_eq!(initial_abs.location.x, 28.0);
    assert_eq!(initial_float.location.y, 130.0);

    set_style_attr(&mut dom, trigger, "width: 11px; height: 1px");
    layout.reflow(&mut dom, &mut style);
    assert_eq!(layout.reflow_counts(), (1, 1));
    let updated_abs = layout
        .tree()
        .box_(layout.tree().box_for_node(abs).unwrap())
        .final_layout;
    let updated_float = layout
        .tree()
        .box_(layout.tree().box_for_node(float).unwrap())
        .final_layout;
    assert_eq!(updated_abs.location.x, initial_abs.location.x);
    assert_eq!(updated_float.location.y, initial_float.location.y);
}

#[test]
fn overflow_pass_records_scrollable_overflow() {
    let (dom, _style, layout) = setup(
        "<div id=scroller style='overflow: hidden; width: 100px; height: 100px'>\
         <div style='width: 300px; height: 250px'></div></div>",
    );
    let scroller = box_of(&layout, &dom, "scroller");
    let ov = layout.tree().box_(scroller).scrollable_overflow;
    assert_eq!(ov.size.width, 300.0);
    assert_eq!(ov.size.height, 250.0);

    // The hidden overflow must not leak into the parent's overflow.
    let body = layout
        .tree()
        .box_for_node(find_element(&dom, "body"))
        .unwrap();
    let body_ov = layout.tree().box_(body).scrollable_overflow;
    assert!(body_ov.size.width <= 784.0 + 1.0);
}

#[test]
fn reflow_stamp_skips_clean_reflows() {
    let mut dom = parse_document("<div id=d>x</div>", ParseOptions::default()).tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut layout = LayoutEngine::new(Viewport::default());
    layout.reflow(&mut dom, &mut style);
    let count_before = layout.tree().box_count();

    // Clean reflow: no work, same tree object state.
    layout.reflow(&mut dom, &mut style);
    assert_eq!(layout.tree().box_count(), count_before);

    // DOM mutation invalidates the stamp.
    let d = find_by_id(&dom, "d");
    dom.set_attribute(
        d,
        oxidepage_dom::node::attr_name(html5ever::local_name!("style")),
        "height: 77px".into(),
    );
    layout.reflow(&mut dom, &mut style);
    let d_box = layout.tree().box_for_node(d).unwrap();
    assert_eq!(layout.tree().box_(d_box).final_layout.size.height, 77.0);
}

#[test]
fn dump_prints_tree() {
    let (_dom, _style, layout) = setup("<div style='width: 100px; height: 20px'>hi</div>");
    let dump = layout.dump();
    assert!(dump.contains("BLOCK"), "dump: {dump}");
    assert!(dump.contains("INLINE"), "dump: {dump}");
    assert!(dump.contains("100x20"), "dump: {dump}");
}

#[test]
fn before_pseudo_takes_space() {
    let mut dom = parse_document(
        "<style>#c::before { content: 'XXXXX'; font-family: Ahem; font-size: 10px; \
         line-height: 10px; display: block }</style>\
         <body style='margin: 0'><div id=c><div id=child style='height: 7px'></div></div></body>",
        ParseOptions::default(),
    )
    .tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let style_el = find_element(&dom, "style");
    let css = dom.text_content(style_el);
    let sheet = style.make_stylesheet(&css, dom.url_extra_data());
    style.add_sheet_for_node(&dom, style_el, sheet);
    let mut layout = LayoutEngine::new(Viewport::default());
    layout.reflow(&mut dom, &mut style);

    // The ::before line (10px tall) pushes the child down.
    let child = box_of(&layout, &dom, "child");
    assert_eq!(layout.tree().box_(child).final_layout.location.y, 10.0);
    let c = box_of(&layout, &dom, "c");
    assert_eq!(layout.tree().box_(c).final_layout.size.height, 17.0);

    // The dump shows the pseudo box.
    let dump = layout.dump();
    assert!(dump.contains("XXXXX"), "dump: {dump}");
}

#[test]
fn table_cells_align_in_columns() {
    let (dom, _style, layout) = setup(
        "<table style='border-collapse: collapse' cellspacing=0>\
         <tr><td id=a style='width: 100px; height: 20px; padding: 0'></td>\
             <td id=b style='width: 50px; height: 20px; padding: 0'></td></tr>\
         <tr><td id=c style='height: 30px; padding: 0'></td>\
             <td id=d style='height: 30px; padding: 0'></td></tr></table>",
    );
    let a = layout.tree().box_(box_of(&layout, &dom, "a")).final_layout;
    let b = layout.tree().box_(box_of(&layout, &dom, "b")).final_layout;
    let c = layout.tree().box_(box_of(&layout, &dom, "c")).final_layout;
    let d = layout.tree().box_(box_of(&layout, &dom, "d")).final_layout;

    // Column widths driven by the first row.
    assert_eq!(a.size.width, 100.0);
    assert_eq!(b.size.width, 50.0);
    // Second-row cells stretch to their column's width.
    assert_eq!(c.size.width, 100.0);
    assert_eq!(d.size.width, 50.0);
    // Same-row cells share a top; second row below the first.
    assert_eq!(a.location.y, b.location.y);
    assert_eq!(b.location.x, a.location.x + 100.0);
    assert_eq!(c.location.y, a.location.y + 20.0);
    assert_eq!(d.location.x, c.location.x + 100.0);
    // Row heights: driven by the tallest cell.
    assert_eq!(c.size.height, 30.0);
}

#[test]
fn table_colspan_spans_tracks() {
    let (dom, _style, layout) = setup(
        "<table style='border-collapse: collapse'>\
         <tr><td id=wide colspan=2 style='height: 10px; padding: 0'></td></tr>\
         <tr><td id=x style='width: 60px; height: 10px; padding: 0'></td>\
             <td id=y style='width: 40px; height: 10px; padding: 0'></td></tr></table>",
    );
    let wide = layout
        .tree()
        .box_(box_of(&layout, &dom, "wide"))
        .final_layout;
    let x = layout.tree().box_(box_of(&layout, &dom, "x")).final_layout;
    let y = layout.tree().box_(box_of(&layout, &dom, "y")).final_layout;
    // The spanning cell covers both columns.
    assert_eq!(wide.size.width, x.size.width + y.size.width);
    assert_eq!(y.location.x, x.location.x + x.size.width);
}

#[test]
fn table_root_box_kind_and_row_has_no_box() {
    let (dom, _style, layout) =
        setup("<table><tr><td id=cell style='width: 10px; height: 10px'></td></tr></table>");
    let table_el = find_element(&dom, "table");
    let table_box = layout.tree().box_for_node(table_el).unwrap();
    assert_eq!(
        layout.tree().box_(table_box).kind,
        oxidepage_layout::BoxKind::TableRoot
    );
    // The cell box is a direct child of the table box.
    let cell_box = box_of(&layout, &dom, "cell");
    assert_eq!(layout.tree().box_(cell_box).parent, Some(table_box));
    // Rows generate no boxes in v1.
    let tr = find_element(&dom, "tr");
    assert!(layout.tree().box_for_node(tr).is_none());
}

// === WP-K: incremental relayout correctness ===

fn set_style_attr(dom: &mut DomTree, node: NodeId, value: &str) {
    dom.set_attribute(
        node,
        oxidepage_dom::node::attr_name(html5ever::local_name!("style")),
        value.into(),
    );
}

#[test]
fn incremental_patch_applies_inline_size_change() {
    let (mut dom, mut style, mut layout) = setup(
        "<body style='margin:0'><div id=a style='width: 100px; height: 10px'></div>\
         <div id=b style='width: 50px; height: 10px'></div></body>",
    );
    let a = find_by_id(&dom, "a");
    let boxes_before = layout.tree().box_count();

    // Style-only change: the box tree is patched, not rebuilt (box ids and
    // count survive), and the new size lands.
    set_style_attr(&mut dom, a, "width: 140px; height: 25px");
    layout.reflow(&mut dom, &mut style);
    assert_eq!(layout.tree().box_count(), boxes_before);
    assert_eq!(layout.reflow_counts(), (1, 1), "patch path must be taken");
    let a_box = layout.tree().box_for_node(a).unwrap();
    let a_layout = layout.tree().box_(a_box).final_layout;
    assert_eq!((a_layout.size.width, a_layout.size.height), (140.0, 25.0));

    // Sibling below moves down accordingly.
    let b = find_by_id(&dom, "b");
    let b_box = layout.tree().box_for_node(b).unwrap();
    assert_eq!(layout.tree().box_(b_box).final_layout.location.y, 25.0);
}

#[test]
fn incremental_patch_bails_on_display_change() {
    let (mut dom, mut style, mut layout) = setup(
        "<div id=a style='width: 100px; height: 10px'></div>\
         <div id=b style='height: 10px'></div>",
    );
    let a = find_by_id(&dom, "a");
    set_style_attr(&mut dom, a, "display: none");
    layout.reflow(&mut dom, &mut style);
    // The box disappears — only a rebuild can do that.
    assert_eq!(
        layout.reflow_counts(),
        (2, 0),
        "display change must rebuild"
    );
    assert!(layout.tree().box_for_node(a).is_none());
    // The sibling moves up to the top of the body.
    let b = find_by_id(&dom, "b");
    let b_box = layout.tree().box_for_node(b).unwrap();
    assert_eq!(layout.tree().box_(b_box).final_layout.location.y, 0.0);
}

#[test]
fn structural_change_still_rebuilds() {
    let (mut dom, mut style, mut layout) = setup("<div id=host></div>");
    let host = find_by_id(&dom, "host");
    let child = dom.create_element(
        oxidepage_dom::node::html_name(html5ever::local_name!("div")),
        vec![],
    );
    dom.append_child(host, child).unwrap();
    set_style_attr(&mut dom, child, "width: 30px; height: 40px");
    layout.reflow(&mut dom, &mut style);
    let child_box = layout.tree().box_for_node(child).expect("new box");
    let l = layout.tree().box_(child_box).final_layout;
    assert_eq!((l.size.width, l.size.height), (30.0, 40.0));
}

#[test]
fn incremental_patch_only_visits_restyled_elements() {
    // The patch is driven by stylo's restyled set, so a style change on one
    // element costs work proportional to that subtree — not to the document.
    let mut html = String::from("<body style='margin:0'>");
    for i in 0..200 {
        html.push_str(&format!(
            "<div id=n{i} style='width:10px; height:10px'></div>"
        ));
    }
    html.push_str("</body>");
    let (mut dom, mut style, mut layout) = setup(&html);

    let target = find_by_id(&dom, "n7");
    set_style_attr(&mut dom, target, "width: 40px; height: 25px");
    // Resolve before the reflow, as `getComputedStyle` does: this consumes
    // stylo's dirty bits, so the restyled set must survive until layout drains
    // it — otherwise the patch below would run empty and leave a stale tree.
    style.resolve_styles(&mut dom);

    // Only the restyled element is reported (its siblings keep their styles),
    // so the patch loop never touches the other 199 divs.
    let restyled = style.restyled_nodes();
    assert!(
        restyled.contains(&target),
        "the changed element must be restyled"
    );
    assert!(
        restyled.len() < 20,
        "a one-element change must not restyle the whole document, got {} nodes",
        restyled.len()
    );

    // The patch still lands the new geometry without a rebuild.
    layout.reflow(&mut dom, &mut style);
    assert_eq!(layout.reflow_counts(), (1, 1), "patch path must be taken");
    let target_box = layout.tree().box_for_node(target).unwrap();
    let l = layout.tree().box_(target_box).final_layout;
    assert_eq!((l.size.width, l.size.height), (40.0, 25.0));
}

#[test]
fn incremental_patch_relayouts_ifc_width() {
    // Changing the width of a text container reflows its lines without a
    // rebuild (shaping is reused; line breaking re-runs).
    let (mut dom, mut style, mut layout) = setup(
        "<div id=t style='font-family: Ahem; font-size: 10px; line-height: 10px; \
         width: 100px'>XXXXX XXXXX</div>",
    );
    let t = find_by_id(&dom, "t");
    let t_box = layout.tree().box_for_node(t).unwrap();
    assert_eq!(layout.tree().box_(t_box).final_layout.size.height, 20.0);

    set_style_attr(
        &mut dom,
        t,
        "font-family: Ahem; font-size: 10px; line-height: 10px; width: 200px",
    );
    layout.reflow(&mut dom, &mut style);
    assert_eq!(layout.reflow_counts(), (1, 1), "width-only change patches");
    let t_box = layout.tree().box_for_node(t).unwrap();
    // Same shaped text now fits one line.
    assert_eq!(layout.tree().box_(t_box).final_layout.size.height, 10.0);
    assert_eq!(layout.tree().box_(t_box).final_layout.size.width, 200.0);
}

#[test]
fn incremental_patch_reshapes_anonymous_block_text_on_font_size_change() {
    // Regression: a mixed block container wraps its text runs ("AAAAA",
    // "BBBBB") in anonymous blocks that have no DOM node. A style-only
    // font-size change on the container takes the patch path, but the patch
    // loop never visits the anonymous blocks, so their captured metrics and
    // pre-shaped parley layout would keep the old 16px size. The container must
    // fall back to a full rebuild so the anon text re-shapes at the new size.
    let (mut dom, mut style, mut layout) = setup(
        "<body style='margin:0'><div id=c style='font-family: Ahem; font-size: 16px; \
         line-height: 1; width: 500px'>AAAAA<div id=block style='height: 5px'></div>\
         BBBBB</div></body>",
    );
    let c = find_by_id(&dom, "c");
    let c_box = layout.tree().box_for_node(c).unwrap();
    // Two 16px text lines around the 5px block: 16 + 5 + 16 = 37.
    assert_eq!(layout.tree().box_(c_box).final_layout.size.height, 37.0);

    set_style_attr(
        &mut dom,
        c,
        "font-family: Ahem; font-size: 40px; line-height: 1; width: 500px",
    );
    layout.reflow(&mut dom, &mut style);
    // The inherited font-size change reaches anonymous-block text, so the
    // container must rebuild rather than silently keep 16px lines.
    assert_eq!(
        layout.reflow_counts(),
        (2, 0),
        "font-size change on an anon-block container rebuilds"
    );
    let c_box = layout.tree().box_for_node(c).unwrap();
    // 40px lines now: 40 + 5 + 40 = 85.
    assert_eq!(layout.tree().box_(c_box).final_layout.size.height, 85.0);
}

#[test]
fn incremental_patch_reshapes_flex_text_run_on_font_size_change() {
    // Analogous hole for a flex container: a bare text run becomes an
    // anonymous flex item (also NodeId-less), so a font-size change must
    // likewise rebuild instead of patching in place.
    let (mut dom, mut style, mut layout) = setup(
        "<body style='margin:0'><div id=f style='display: flex; font-family: Ahem; \
         font-size: 16px; line-height: 1; width: 500px'>AAAAA</div></body>",
    );
    let f = find_by_id(&dom, "f");
    let f_box = layout.tree().box_for_node(f).unwrap();
    assert_eq!(layout.tree().box_(f_box).final_layout.size.height, 16.0);

    set_style_attr(
        &mut dom,
        f,
        "display: flex; font-family: Ahem; font-size: 40px; line-height: 1; width: 500px",
    );
    layout.reflow(&mut dom, &mut style);
    assert_eq!(
        layout.reflow_counts(),
        (2, 0),
        "font-size change on a flex text-run container rebuilds"
    );
    let f_box = layout.tree().box_for_node(f).unwrap();
    assert_eq!(layout.tree().box_(f_box).final_layout.size.height, 40.0);
}

// === Code-review regression tests (Phase 5 review) ===

#[test]
fn img_with_attr_ratio_and_max_width_keeps_finite_height() {
    // Review #1: 0×0 intrinsic size must not leak NaN through the max-width
    // clamp; the ratio comes from the width/height attributes.
    let (dom, _style, layout) = setup(
        "<body style='margin:0'><div style='width: 300px'>\
         <img id=i width=800 height=600 style='max-width: 100%'></div></body>",
    );
    let img = layout.tree().box_(box_of(&layout, &dom, "i")).final_layout;
    assert_eq!(img.size.width, 300.0);
    assert_eq!(img.size.height, 225.0, "300 / (800/600)");
}

#[test]
fn img_without_any_ratio_source_is_zero_not_nan() {
    let (dom, _style, layout) = setup(
        "<body style='margin:0'><div style='width: 300px'>\
         <img id=i style='max-width: 100%'></div></body>",
    );
    let img = layout.tree().box_(box_of(&layout, &dom, "i")).final_layout;
    assert_eq!((img.size.width, img.size.height), (0.0, 0.0));
    assert!(img.size.height.is_finite());
}

#[test]
fn inline_canvas_is_atomic_replaced_box() {
    // Review #3: <canvas> is replaced but was missing from the atomic
    // inline tag set, collapsing to zero inside an IFC.
    let (dom, _style, layout) = setup(
        "<body style='margin:0'><p style='font-family: Ahem; font-size: 10px; \
         line-height: 10px'>x<canvas id=c width=100 height=50></canvas></p></body>",
    );
    let c = layout.tree().box_(box_of(&layout, &dom, "c")).final_layout;
    assert_eq!((c.size.width, c.size.height), (100.0, 50.0));
    assert_eq!(c.location.x, 10.0, "placed after the 10px glyph");
}

#[test]
fn huge_colspan_does_not_overflow_or_panic() {
    // Review #5: colspan clamps to 1000 and the column counter is u32.
    let (dom, _style, layout) = setup(
        "<table><tr><td id=a colspan=40000 style='height: 5px'></td>\
         <td colspan=40000 style='height: 5px'></td></tr></table>",
    );
    let a = layout.tree().box_(box_of(&layout, &dom, "a")).final_layout;
    assert!(a.size.width.is_finite());
}

#[test]
fn textarea_negative_rows_clamps_to_default() {
    // Review #6: rows must be >= 1; invalid values fall back to 2 rows.
    let (dom, _style, layout) = setup(
        "<body style='margin:0'><textarea id=t rows='-3' \
         style='font-family: Ahem; font-size: 10px; line-height: 10px'></textarea></body>",
    );
    let t = layout.tree().box_(box_of(&layout, &dom, "t")).final_layout;
    assert_eq!(t.size.height, 20.0, "two default rows of 10px");
}

/// A `min-height` on an auto-height block is a lower bound on its own size; it
/// is not a definite height for its children's percentages to resolve against.
/// Taffy folds it into the percentage basis it passes down, which collapsed
/// mgid.com's whole page onto `main`'s `min-height` (the footer ended up under
/// the hero).
#[test]
fn min_height_is_not_a_percentage_basis_for_children() {
    let (dom, _style, layout) = setup(
        "<div style='display:flex;flex-direction:column'>\
           <div id=item style='flex:1 1 0%;min-height:480px'>\
             <div id=pct style='height:100%'>\
               <div id=tall style='height:900px'></div>\
             </div>\
           </div>\
           <div id=foot style='height:80px'></div>\
         </div>",
    );

    let height = |id| {
        layout
            .tree()
            .box_(box_of(&layout, &dom, id))
            .final_layout
            .size
            .height
    };
    assert_eq!(height("tall"), 900.0);
    // The percentage height resolves as `auto`, so both ancestors take the
    // content's height rather than the 480px minimum.
    assert_eq!(height("pct"), 900.0);
    assert_eq!(height("item"), 900.0);
    assert_eq!(height("foot"), 80.0);
}

// === ADR-0013 review regression tests ===

/// A `min-height` floor is applied to taffy's border-box output. Under the
/// default `content-box` sizing the minimum bounds the *content* box, so the
/// border-box floor must add the vertical padding + border: 100px min + 2×30px
/// padding = 160px, not 100px.
#[test]
fn auto_height_min_height_floor_accounts_for_content_box_padding() {
    let (dom, _style, layout) = setup(
        "<body style='margin:0'>\
         <div id=d style='min-height: 100px; padding: 30px'></div></body>",
    );
    let d = layout.tree().box_(box_of(&layout, &dom, "d")).final_layout;
    assert_eq!(d.size.height, 160.0);
}

/// With `box-sizing: border-box` the `min-height` already includes the padding,
/// so the border-box floor is exactly the minimum (no double counting).
#[test]
fn auto_height_min_height_floor_border_box_is_not_double_counted() {
    let (dom, _style, layout) = setup(
        "<body style='margin:0'>\
         <div id=d style='box-sizing: border-box; min-height: 100px; padding: 30px'>\
         </div></body>",
    );
    let d = layout.tree().box_(box_of(&layout, &dom, "d")).final_layout;
    assert_eq!(d.size.height, 100.0);
}

/// A non-`none` `transform` establishes a containing block for an absolutely
/// positioned descendant, even on a `position: static` element: the abs child
/// is hoisted onto the transformed box, not onto a farther positioned ancestor.
#[test]
fn transformed_static_element_is_containing_block_for_absolute_child() {
    let (dom, _style, layout) = setup(
        "<body style='margin:0'>\
         <div id=outer style='position: relative; width: 300px; padding-top: 50px'>\
           <div id=mid style='transform: translateX(100px); height: 100px'>\
             <div id=abs style='position: absolute; top: 0; left: 0; \
                  width: 50px; height: 60px'></div>\
           </div>\
         </div></body>",
    );
    let mid_box = box_of(&layout, &dom, "mid");
    let abs_box = box_of(&layout, &dom, "abs");
    assert_eq!(
        layout.tree().box_(abs_box).parent,
        Some(mid_box),
        "abs child is contained by the transformed #mid, not #outer"
    );
    // top:0/left:0 place it at #mid's padding-box origin.
    let abs = layout.tree().box_(abs_box).final_layout;
    assert_eq!((abs.location.x, abs.location.y), (0.0, 0.0));
}

/// A `transform` captures even `position: fixed` descendants (unlike
/// `position: relative`), so the fixed child is contained by the transformed
/// box rather than pinned to the viewport root.
#[test]
fn transformed_element_is_containing_block_for_fixed_child() {
    let (dom, _style, layout) = setup(
        "<body style='margin:0'>\
         <div id=mid style='transform: translateY(10px); width: 200px; height: 100px'>\
           <div id=fix style='position: fixed; top: 5px; left: 5px; \
                width: 10px; height: 10px'></div>\
         </div></body>",
    );
    let mid_box = box_of(&layout, &dom, "mid");
    let fix_box = box_of(&layout, &dom, "fix");
    assert_eq!(layout.tree().box_(fix_box).parent, Some(mid_box));
}

/// `position: relative` does not capture a `position: fixed` descendant — only
/// a transform-like property does — so the fixed box stays at the root.
#[test]
fn fixed_child_ignores_relative_ancestor() {
    let (dom, _style, layout) = setup(
        "<body style='margin:0'>\
         <div id=rel style='position: relative; width: 200px; height: 100px'>\
           <div id=fix style='position: fixed; top: 5px; left: 5px; \
                width: 10px; height: 10px'></div>\
         </div></body>",
    );
    let rel_box = box_of(&layout, &dom, "rel");
    let fix_box = box_of(&layout, &dom, "fix");
    assert_ne!(layout.tree().box_(fix_box).parent, Some(rel_box));
}

/// An abs box with auto insets nested inside another abs box with auto insets:
/// the inner box's static position must be read from the outer box's *corrected*
/// location. Tree-order correction with immediate application guarantees the
/// outer box is fixed before the inner box reads it.
#[test]
fn nested_auto_inset_absolutes_use_corrected_ancestor_position() {
    let (dom, _style, layout) = setup(
        "<body style='margin:0'>\
         <div id=cb style='position: relative; width: 300px; height: 300px; padding: 40px'>\
           <div id=outer style='position: absolute; width: 100px; height: 100px'>\
             <div id=inner style='position: absolute; width: 10px; height: 10px'></div>\
           </div>\
         </div></body>",
    );
    // #outer (auto insets) lands at its static parent #cb's content origin,
    // i.e. the 40px padding offset, relative to #cb.
    let outer = layout
        .tree()
        .box_(box_of(&layout, &dom, "outer"))
        .final_layout;
    assert_eq!((outer.location.x, outer.location.y), (40.0, 40.0));
    // #inner (auto insets) sits at #outer's content origin. #outer has no
    // padding, so relative to #outer it is (0,0) — which only holds if #inner
    // read #outer's *corrected* position.
    let inner = layout
        .tree()
        .box_(box_of(&layout, &dom, "inner"))
        .final_layout;
    assert_eq!((inner.location.x, inner.location.y), (0.0, 0.0));
}

/// Box-tree construction recurses once per DOM nesting level; without a depth
/// cap a pathologically deep DOM overflows the stack. The cap keeps the box
/// tree bounded regardless of DOM depth, so every downstream pass (taffy,
/// rounding, paint) stays within a bounded tree.
///
/// The whole reflow runs on a large-stack thread because *style resolution* —
/// a separate pass owned by the style crate — also recurses on DOM depth and
/// would overflow the 2 MiB test-thread stack before construction even runs;
/// that is out of scope here. What this test pins is the construction cap: with
/// the fix the box tree is ~256 boxes deep; without it the box tree would mirror
/// the multi-thousand DOM depth.
#[test]
fn deeply_nested_dom_construction_depth_is_capped() {
    let count = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(|| {
            // Far deeper than the construction cap, built by the parser
            // (iterative) so the deep DOM itself is cheap to create.
            let html = "<div>".repeat(4000);
            let mut dom = parse_document(&html, ParseOptions::default()).tree;
            let mut style = StyleEngine::new(&dom, Viewport::default());
            let mut layout = LayoutEngine::new(Viewport::default());
            layout.reflow(&mut dom, &mut style);
            layout.tree().box_count()
        })
        .unwrap()
        .join()
        .unwrap();

    // The linear DOM makes box_count equal the box-tree depth (+ root chrome),
    // so a count near the cap proves construction stopped descending; an
    // uncapped build would produce thousands of boxes.
    assert!(
        (256..512).contains(&count),
        "construction depth must be capped near 256, got {count} boxes"
    );
}

// === CSS multi-column (ADR-0016) ===
//
// Ahem at `font-size: N; line-height: N` makes every glyph an N×N square and
// every line exactly N tall, so column heights and line tops are exact integers.

/// `(used_width, used_gap, columns)` of the multicol container `#id`.
fn columns_of(
    layout: &LayoutEngine,
    dom: &DomTree,
    id_attr: &str,
) -> (f32, f32, Vec<oxidepage_layout::ColumnRange>) {
    let box_id = box_of(layout, dom, id_attr);
    let mc = layout
        .tree()
        .box_(box_id)
        .multicol
        .as_deref()
        .unwrap_or_else(|| panic!("#{id_attr} is not a multicol container"));
    (mc.used_width(), mc.used_gap(), mc.columns().to_vec())
}

/// A 2-column Ahem paragraph: four 100px lines of 20px text in a 200px box, so
/// the balanced column height is 40px (two lines each).
const TWO_COLUMN_TEXT: &str = "<div id=m style='font-family: Ahem; font-size: 20px; \
     line-height: 20px; width: 200px; column-count: 2; column-gap: 0'>\
     XXXXX XXXXX XXXXX XXXXX</div>";

#[test]
fn multicol_wraps_its_content_in_an_anonymous_flow_box() {
    let (dom, _style, layout) = setup(TWO_COLUMN_TEXT);
    let m = box_of(&layout, &dom, "m");

    let root = layout.tree().box_(m);
    assert_eq!(root.kind, oxidepage_layout::BoxKind::MulticolRoot);
    assert_eq!(root.children.len(), 1, "exactly one anonymous flow child");

    // The flow is anonymous, so the element still maps to its *root* box.
    let flow = root.children[0];
    assert_eq!(layout.tree().box_(flow).dom_node, None);
    assert_eq!(layout.tree().multicol_root_of_flow(flow), Some(m));
}

#[test]
fn column_count_splits_the_content_width() {
    let (dom, _style, layout) = setup(
        "<div id=m style='font-family: Ahem; font-size: 20px; line-height: 20px; \
         width: 300px; column-count: 3; column-gap: 0'>XXXXX XXXXX XXXXX</div>",
    );
    let (width, gap, columns) = columns_of(&layout, &dom, "m");
    assert_eq!((width, gap), (100.0, 0.0));
    assert_eq!(columns.len(), 3);
    assert_eq!(
        columns.iter().map(|c| c.x).collect::<Vec<_>>(),
        [0.0, 100.0, 200.0]
    );
}

#[test]
fn column_width_derives_the_used_count() {
    // floor((320 + 20) / (100 + 20)) = 2 columns, each (320 - 20) / 2 = 150px.
    let (dom, _style, layout) = setup(
        "<div id=m style='font-family: Ahem; font-size: 20px; line-height: 20px; \
         width: 320px; column-width: 100px; column-gap: 20px'>XXX XXX XXX XXX</div>",
    );
    let (width, gap, columns) = columns_of(&layout, &dom, "m");
    assert_eq!((width, gap), (150.0, 20.0));
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[1].x, 170.0);
}

#[test]
fn column_gap_normal_is_one_em() {
    // `column-gap: normal` is 1em — *not* the 0px `stylo_taffy` maps it to.
    let (dom, _style, layout) = setup(
        "<div id=m style='font-family: Ahem; font-size: 20px; line-height: 20px; \
         width: 220px; column-count: 2'>XXXXX XXXXX XXXXX</div>",
    );
    let (width, gap, _) = columns_of(&layout, &dom, "m");
    assert_eq!(gap, 20.0);
    assert_eq!(width, 100.0);
}

#[test]
fn text_balances_into_equal_columns() {
    let (dom, _style, layout) = setup(TWO_COLUMN_TEXT);
    let (width, _, columns) = columns_of(&layout, &dom, "m");
    assert_eq!(width, 100.0);

    // Four 20px lines: 80px of flow, balanced into two 40px columns.
    assert_eq!(columns.len(), 2);
    assert_eq!((columns[0].start, columns[0].end), (0.0, 40.0));
    assert_eq!((columns[1].start, columns[1].end), (40.0, 80.0));
    // Every boundary is a line top: no line is ever cut in half.
    for column in &columns {
        assert_eq!(column.start % 20.0, 0.0, "{column:?} splits a line");
    }

    // The container is as tall as its tallest column, not as the flow.
    let m = box_of(&layout, &dom, "m");
    assert_eq!(layout.tree().box_(m).final_layout.size.height, 40.0);
}

#[test]
fn unbreakable_content_floors_the_column_height() {
    // One monolithic 100px block cannot be split, so it sets the column height
    // and the second column stays empty (CSS Multicol §3.3).
    let (dom, _style, layout) = setup(
        "<div id=m style='width: 200px; column-count: 2; column-gap: 0'>\
         <div style='height: 100px'></div></div>",
    );
    let (_, _, columns) = columns_of(&layout, &dom, "m");
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].end, 100.0);

    let m = box_of(&layout, &dom, "m");
    assert_eq!(layout.tree().box_(m).final_layout.size.height, 100.0);
}

#[test]
fn a_definite_height_fills_instead_of_balancing() {
    // 80px of content in a 40px-tall container: fill each column to 40px rather
    // than balancing (the `column-fill: auto` behaviour).
    let (dom, _style, layout) = setup(
        "<div id=m style='font-family: Ahem; font-size: 20px; line-height: 20px; \
         width: 200px; height: 40px; column-count: 3; column-gap: 0'>\
         XXXXX XXXXX XXXXX XXXXX</div>",
    );
    let (_, _, columns) = columns_of(&layout, &dom, "m");
    assert_eq!(columns.len(), 2, "two full columns, the third unused");
    assert_eq!((columns[0].start, columns[0].end), (0.0, 40.0));
    assert_eq!((columns[1].start, columns[1].end), (40.0, 80.0));

    let m = box_of(&layout, &dom, "m");
    assert_eq!(layout.tree().box_(m).final_layout.size.height, 40.0);
}

#[test]
fn the_tall_flow_does_not_leak_into_scrollable_overflow() {
    // The flow is twice as tall as the container it is columnized into; if it
    // contributed to overflow, the *document* would be that tall.
    let (dom, _style, layout) = setup(TWO_COLUMN_TEXT);
    let m = box_of(&layout, &dom, "m");
    let root = layout.tree().box_(m);
    assert_eq!(
        root.scrollable_overflow.max_y(),
        root.final_layout.size.height
    );

    let html = find_element(&dom, "html");
    let (_, scroll_height) = layout.scroll_size(html).unwrap();
    // Body margin 8 + the 40px container: nowhere near the 80px flow.
    assert_eq!(
        scroll_height, 600.0,
        "the viewport, not the un-columnized flow"
    );
}

#[test]
fn a_block_in_the_second_column_reports_a_rect_there() {
    // Three 30px blocks balance into two columns of 60px: #b3 starts the second.
    let (dom, _style, layout) = setup(
        "<body style='margin: 0'><div id=m style='width: 200px; column-count: 2; \
         column-gap: 0'><div style='height: 30px'></div><div style='height: 30px'></div>\
         <div id=b3 style='height: 30px'></div></div></body>",
    );
    let (width, _, columns) = columns_of(&layout, &dom, "m");
    assert_eq!(width, 100.0);
    assert_eq!(columns.len(), 2);
    assert_eq!((columns[0].start, columns[0].end), (0.0, 60.0));

    // Flow-space y = 60 → column 2, at its top.
    let rect = layout.border_box(find_by_id(&dom, "b3")).unwrap();
    assert_eq!((rect.origin.x, rect.origin.y), (100.0, 0.0));
}

#[test]
fn an_inline_split_across_a_column_break_reports_one_rect_per_column() {
    let (dom, _style, layout) = setup(
        "<body style='margin: 0'><div id=m style='font-family: Ahem; font-size: 20px; \
         line-height: 20px; width: 200px; column-count: 2; column-gap: 0'>\
         <span id=s>XXXXX XXXXX XXXXX XXXXX</span></div></body>",
    );
    let rects = layout.client_rects(&dom, find_by_id(&dom, "s"));
    // Four lines: two in each column.
    assert_eq!(rects.len(), 4);
    assert_eq!(
        rects
            .iter()
            .map(|r| (r.origin.x, r.origin.y))
            .collect::<Vec<_>>(),
        [(0.0, 0.0), (0.0, 20.0), (100.0, 0.0), (100.0, 20.0)]
    );
}

#[test]
fn hit_testing_descends_into_the_column_under_the_point() {
    let (dom, _style, layout) = setup(
        "<body style='margin: 0'><div id=m style='width: 200px; column-count: 2; \
         column-gap: 20px'><div style='height: 30px'></div>\
         <div id=b2 style='height: 30px'></div></div></body>",
    );
    let (width, gap, columns) = columns_of(&layout, &dom, "m");
    assert_eq!((width, gap), (90.0, 20.0));
    assert_eq!(columns.len(), 2);

    // #b2 shows in the second column, which starts at x = 110.
    let b2 = find_by_id(&dom, "b2");
    assert_eq!(layout.element_from_point(&dom, 120.0, 10.0), Some(b2));
    // A point in the gap hits the container, never its content.
    let m = find_by_id(&dom, "m");
    assert_eq!(layout.element_from_point(&dom, 100.0, 10.0), Some(m));
}

#[test]
fn column_property_changes_force_a_box_tree_rebuild() {
    let (mut dom, mut style, mut layout) = setup(
        "<div id=m style='font-family: Ahem; font-size: 20px; line-height: 20px; \
         width: 200px; column-count: 2; column-gap: 0'>XXXXX XXXXX XXXXX XXXXX</div>",
    );
    let (rebuilds, _) = layout.reflow_counts();
    let m = find_by_id(&dom, "m");

    // `column-count` decides the box structure: it cannot be patched in place.
    set_style_attr(
        &mut dom,
        m,
        "font-family: Ahem; font-size: 20px; line-height: 20px; width: 200px; \
         column-count: 4; column-gap: 0",
    );
    layout.reflow(&mut dom, &mut style);
    assert_eq!(layout.reflow_counts().0, rebuilds + 1);

    // So does `column-gap`, which lives on the multicol context, out of reach of
    // a taffy-style patch.
    set_style_attr(
        &mut dom,
        m,
        "font-family: Ahem; font-size: 20px; line-height: 20px; width: 200px; \
         column-count: 4; column-gap: 10px",
    );
    layout.reflow(&mut dom, &mut style);
    assert_eq!(layout.reflow_counts().0, rebuilds + 2);
    assert_eq!(columns_of(&layout, &dom, "m").1, 10.0);
}

#[test]
fn a_font_size_change_on_a_multicol_root_reshapes_its_flow() {
    // The flow box is anonymous, so the patch loop never visits it: its captured
    // metrics and pre-shaped parley layout would go stale. Must rebuild.
    let (mut dom, mut style, mut layout) = setup(TWO_COLUMN_TEXT);
    let (rebuilds, _) = layout.reflow_counts();

    let m = find_by_id(&dom, "m");
    set_style_attr(
        &mut dom,
        m,
        "font-family: Ahem; font-size: 10px; line-height: 10px; width: 200px; \
         column-count: 2; column-gap: 0",
    );
    layout.reflow(&mut dom, &mut style);
    assert_eq!(layout.reflow_counts().0, rebuilds + 1);

    // 10px Ahem: still four lines, but each is 10px, so the flow is 40px and the
    // balanced column height halves to 20px.
    let (_, _, columns) = columns_of(&layout, &dom, "m");
    assert_eq!(columns.len(), 2);
    assert_eq!((columns[0].start, columns[0].end), (0.0, 20.0));
    assert_eq!((columns[1].start, columns[1].end), (20.0, 40.0));
}
