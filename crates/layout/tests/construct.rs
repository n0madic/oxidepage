//! WP-C: box tree construction tests on resolved DOM trees.

use oxidepage_dom::select::enter_active_tree;
use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};
use oxidepage_layout::construct::build_layout_tree;
use oxidepage_layout::{BoxKind, FontSystem, LayoutTree, ReplacedContent};
use oxidepage_style::{StyleEngine, Viewport};

fn parse(html: &str) -> DomTree {
    parse_document(html, ParseOptions::default()).tree
}

fn find_element(tree: &DomTree, local: &str) -> NodeId {
    tree.inclusive_descendants(tree.document())
        .find(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| &*el.name.local == local)
        })
        .unwrap_or_else(|| panic!("no <{local}> in document"))
}

fn layout(html: &str) -> (DomTree, LayoutTree) {
    let mut dom = parse(html);
    let mut engine = StyleEngine::new(&dom, Viewport::default());
    engine.resolve_styles(&mut dom);
    let mut fonts = FontSystem::new();
    let tree = {
        let _scope = enter_active_tree(&dom);
        build_layout_tree(
            &dom,
            &engine,
            &mut fonts,
            Viewport::default(),
            &oxidepage_layout::ImageStore::default(),
        )
    };
    (dom, tree)
}

#[test]
fn block_children_get_block_boxes() {
    let (dom, tree) = layout("<div id=a></div><div id=b></div>");
    let body = tree.box_for_node(find_element(&dom, "body")).unwrap();
    let body_box = tree.box_(body);
    assert_eq!(body_box.kind, BoxKind::Block);
    assert_eq!(body_box.children.len(), 2);
    for &child in &body_box.children {
        assert_eq!(tree.box_(child).kind, BoxKind::Block);
        assert_eq!(tree.box_(child).parent, Some(body));
    }
}

#[test]
fn text_content_makes_inline_root_with_shaped_ifc() {
    let (dom, tree) = layout("<div>hello world</div>");
    let div = tree.box_for_node(find_element(&dom, "div")).unwrap();
    let div_box = tree.box_(div);
    assert_eq!(div_box.kind, BoxKind::InlineRoot);
    let ifc = div_box.ifc.as_ref().expect("inline root has IFC data");
    assert_eq!(ifc.text, "hello world");
    assert!(div_box.children.is_empty(), "no atomic inline boxes");
}

#[test]
fn nested_inline_spans_are_contributors() {
    let (dom, tree) = layout("<div>a<span>b</span>c</div>");
    let div = tree.box_for_node(find_element(&dom, "div")).unwrap();
    let span = find_element(&dom, "span");
    let ifc = tree.box_(div).ifc.as_ref().unwrap();
    assert_eq!(ifc.text, "abc");
    assert!(ifc.contributors.contains(&span));
    // The span generates no box of its own; it lives inside the IFC.
    assert!(tree.box_for_node(span).is_none());
}

#[test]
fn trailing_whitespace_inside_inline_collapses_but_survives() {
    // Regression: parley trims trailing whitespace at every style-span
    // boundary, so `<span>AAA </span><span>BBB </span>` used to render as
    // "AAABBB". Per CSS an inline element's trailing whitespace collapses
    // with the following in-flow content into a single space.
    let (dom, tree) = layout("<div>[<span>AAA </span><span>BBB </span>CCC <span>DDD</span>]</div>");
    let div = tree.box_for_node(find_element(&dom, "div")).unwrap();
    let ifc = tree.box_(div).ifc.as_ref().unwrap();
    assert_eq!(ifc.text, "[AAA BBB CCC DDD]");
}

#[test]
fn whitespace_between_adjacent_inlines_collapses_to_one_space() {
    // A trailing space in one span meeting a leading space in the next must
    // collapse to exactly one space, not two and not zero.
    let (dom, tree) = layout("<div>[<span>A </span><span> B</span>]</div>");
    let div = tree.box_for_node(find_element(&dom, "div")).unwrap();
    let ifc = tree.box_(div).ifc.as_ref().unwrap();
    assert_eq!(ifc.text, "[A B]");
}

#[test]
fn mixed_content_gets_anonymous_block() {
    let (dom, tree) = layout("<div id=outer>text<div id=inner></div></div>");
    let outer = tree.box_for_node(find_element(&dom, "div")).unwrap();
    let outer_box = tree.box_(outer);
    assert_eq!(outer_box.kind, BoxKind::Block);
    assert_eq!(outer_box.children.len(), 2);

    let anon = tree.box_(outer_box.children[0]);
    assert_eq!(anon.kind, BoxKind::AnonymousBlock);
    assert_eq!(anon.dom_node, None);
    assert_eq!(anon.ifc.as_ref().unwrap().text, "text");

    let inner = tree.box_(outer_box.children[1]);
    assert_eq!(inner.kind, BoxKind::Block);
}

#[test]
fn display_none_generates_no_box() {
    let (dom, tree) = layout("<div style='display:none'><p>x</p></div><span>y</span>");
    let div = find_element(&dom, "div");
    let p = find_element(&dom, "p");
    assert!(tree.box_for_node(div).is_none());
    assert!(tree.box_for_node(p).is_none());
}

#[test]
fn display_contents_hoists_children() {
    let (dom, tree) = layout("<div id=host style='display:contents'><p>a</p><p>b</p></div>");
    let body = tree.box_for_node(find_element(&dom, "body")).unwrap();
    let host = find_element(&dom, "div");
    assert!(tree.box_for_node(host).is_none(), "contents: no box");
    // Both <p> boxes hoisted to the body.
    assert_eq!(tree.box_(body).children.len(), 2);
}

#[test]
fn img_is_replaced_with_attr_sizes() {
    let (dom, tree) = layout("<img width=100 height=50>");
    let img = tree.box_for_node(find_element(&dom, "img")).unwrap();
    let img_box = tree.box_(img);
    assert_eq!(img_box.kind, BoxKind::Replaced);
    match &img_box.replaced {
        Some(ReplacedContent::Image(ctx)) => {
            assert_eq!(ctx.attr_size.width, Some(100.0));
            assert_eq!(ctx.attr_size.height, Some(50.0));
        }
        other => panic!("expected image replaced content, got {other:?}"),
    }
}

#[test]
fn inline_with_block_descendant_is_block_child() {
    // The span contains a block, so the container must not become an inline
    // root; the span is treated as a block-level child (ADR-0006 §4).
    let (dom, tree) = layout("<div id=c>x<span id=s><p>inner</p></span></div>");
    let container = tree.box_for_node(find_element(&dom, "div")).unwrap();
    let container_box = tree.box_(container);
    assert_eq!(container_box.kind, BoxKind::Block);
    // Children: anonymous block for "x", then the span as a block child.
    assert_eq!(container_box.children.len(), 2);
    assert_eq!(
        tree.box_(container_box.children[0]).kind,
        BoxKind::AnonymousBlock
    );
    let span = find_element(&dom, "span");
    assert_eq!(tree.box_for_node(span), Some(container_box.children[1]));
}

#[test]
fn atomic_inline_boxes_inside_ifc() {
    let (dom, tree) = layout("<div>a<span id=at style='display:inline-block'>b</span>c</div>");
    let div = tree.box_for_node(find_element(&dom, "div")).unwrap();
    let div_box = tree.box_(div);
    assert_eq!(div_box.kind, BoxKind::InlineRoot);
    // The atomic inline generates its own child box…
    assert_eq!(div_box.children.len(), 1);
    let atomic = tree.box_(div_box.children[0]);
    assert_eq!(atomic.kind, BoxKind::InlineRoot); // it has its own text IFC
    // …and a placeholder in the parley layout.
    let ifc = div_box.ifc.as_ref().unwrap();
    assert_eq!(ifc.layout.inline_boxes().len(), 1);
    assert_eq!(
        ifc.layout.inline_boxes()[0].id,
        div_box.children[0].index() as u64
    );
    assert_eq!(ifc.text, "ac");
}

#[test]
fn whitespace_between_blocks_is_dropped() {
    let (dom, tree) = layout("<div>  <p>a</p>\n  <p>b</p>  </div>");
    let div = tree.box_for_node(find_element(&dom, "div")).unwrap();
    let div_box = tree.box_(div);
    assert_eq!(div_box.kind, BoxKind::Block);
    assert_eq!(div_box.children.len(), 2, "whitespace runs are not wrapped");
}

#[test]
fn flex_container_wraps_text_in_anonymous_item() {
    let (dom, tree) = layout("<div id=f style='display:flex'>text<span id=s>item</span></div>");
    let flex = tree.box_for_node(find_element(&dom, "div")).unwrap();
    let flex_box = tree.box_(flex);
    assert_eq!(flex_box.children.len(), 2);
    // "text" gets an anonymous wrapper; the span becomes an item directly.
    assert_eq!(
        tree.box_(flex_box.children[0]).kind,
        BoxKind::AnonymousBlock
    );
    let span = find_element(&dom, "span");
    assert_eq!(tree.box_for_node(span), Some(flex_box.children[1]));
    assert_eq!(tree.box_(flex_box.children[1]).kind, BoxKind::InlineRoot);
}

#[test]
fn br_contributes_newline() {
    let (dom, tree) = layout("<div>a<br>b</div>");
    let div = tree.box_for_node(find_element(&dom, "div")).unwrap();
    let ifc = tree.box_(div).ifc.as_ref().unwrap();
    assert_eq!(ifc.text, "a\nb");
    assert!(tree.box_(div).children.is_empty());
}

#[test]
fn text_input_is_replaced_leaf() {
    let (dom, tree) = layout("<div><input type=text></div>");
    let input = tree.box_for_node(find_element(&dom, "input")).unwrap();
    match tree.box_(input).replaced {
        Some(ReplacedContent::TextInput { multiline, .. }) => assert!(!multiline),
        ref other => panic!("expected text input, got {other:?}"),
    }
    assert_eq!(tree.box_(input).kind, BoxKind::Replaced);
}

#[test]
fn hidden_input_generates_no_box() {
    let (dom, tree) = layout("<div><input type=hidden></div>");
    let input = find_element(&dom, "input");
    assert!(tree.box_for_node(input).is_none());
}

#[test]
fn before_pseudo_in_inline_context_prepends_text() {
    let mut dom = parse(
        "<style>#d::before { content: 'AB' } #d::after { content: 'YZ' }</style>\
         <div id=d>mid</div>",
    );
    let mut engine = StyleEngine::new(&dom, Viewport::default());
    let style_el = find_element(&dom, "style");
    let css = dom.text_content(style_el);
    let sheet = engine.make_stylesheet(&css, dom.url_extra_data());
    engine.add_sheet_for_node(&dom, style_el, sheet);
    engine.resolve_styles(&mut dom);
    let mut fonts = FontSystem::new();
    let tree = {
        let _scope = enter_active_tree(&dom);
        build_layout_tree(
            &dom,
            &engine,
            &mut fonts,
            Viewport::default(),
            &oxidepage_layout::ImageStore::default(),
        )
    };

    let div = tree.box_for_node(find_element(&dom, "div")).unwrap();
    let div_box = tree.box_(div);
    assert_eq!(div_box.kind, BoxKind::InlineRoot);
    assert_eq!(div_box.ifc.as_ref().unwrap().text, "ABmidYZ");
}

#[test]
fn pseudo_boxes_flank_block_children() {
    let mut dom = parse(
        "<style>#c::before { content: 'B' } #c::after { content: 'A' }</style>\
         <div id=c><p>block</p></div>",
    );
    let mut engine = StyleEngine::new(&dom, Viewport::default());
    let style_el = find_element(&dom, "style");
    let css = dom.text_content(style_el);
    let sheet = engine.make_stylesheet(&css, dom.url_extra_data());
    engine.add_sheet_for_node(&dom, style_el, sheet);
    engine.resolve_styles(&mut dom);
    let mut fonts = FontSystem::new();
    let tree = {
        let _scope = enter_active_tree(&dom);
        build_layout_tree(
            &dom,
            &engine,
            &mut fonts,
            Viewport::default(),
            &oxidepage_layout::ImageStore::default(),
        )
    };

    let container = find_element(&dom, "div");
    let c_box_id = tree.box_for_node(container).unwrap();
    let c_box = tree.box_(c_box_id);
    assert_eq!(c_box.children.len(), 3);

    let before = tree.box_(c_box.children[0]);
    assert_eq!(
        before.pseudo,
        Some(oxidepage_layout::tree::PseudoBox::Before)
    );
    assert_eq!(before.dom_node, Some(container));
    assert_eq!(before.ifc.as_ref().unwrap().text, "B");

    let after = tree.box_(c_box.children[2]);
    assert_eq!(after.pseudo, Some(oxidepage_layout::tree::PseudoBox::After));
    assert_eq!(after.ifc.as_ref().unwrap().text, "A");

    // Pseudo boxes must not shadow the owner's principal box in the map.
    assert_eq!(tree.box_for_node(container), Some(c_box_id));
}

#[test]
fn text_transform_capitalize_uppercases_word_starts() {
    // Review #7: capitalize was silently dropped (only upper/lowercase
    // were handled in the IFC walk).
    let (dom, tree) = layout("<div style='text-transform: capitalize'>hello brave world</div>");
    let div = tree.box_for_node(find_element(&dom, "div")).unwrap();
    assert_eq!(
        tree.box_(div).ifc.as_ref().unwrap().text,
        "Hello Brave World"
    );
}
