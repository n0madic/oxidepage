//! Tests for the Phase 2 DOM additions: pins, cloning, normalize,
//! isEqualNode, compareDocumentPosition, and selector matching.

use oxidepage_dom::serialize::{inner_html, outer_html};
use oxidepage_dom::tree::{
    DOCUMENT_POSITION_CONTAINED_BY, DOCUMENT_POSITION_CONTAINS, DOCUMENT_POSITION_DISCONNECTED,
    DOCUMENT_POSITION_FOLLOWING, DOCUMENT_POSITION_PRECEDING,
};
use oxidepage_dom::{DomTree, NodeData, ParseOptions, parse_document, parse_selector_list};

fn parse(html: &str) -> DomTree {
    parse_document(html, ParseOptions::default()).tree
}

fn find_element(tree: &DomTree, local: &str) -> oxidepage_dom::NodeId {
    tree.inclusive_descendants(tree.document())
        .find(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| &*el.name.local == local)
        })
        .unwrap_or_else(|| panic!("no <{local}> in document"))
}

// === Pins ===

#[test]
fn pinned_detached_subtree_is_not_freed() {
    let mut tree = parse("<div><span>hi</span></div>");
    let div = find_element(&tree, "div");
    let span = find_element(&tree, "span");

    tree.pin(span);
    tree.remove(div);
    // The detached tree contains a pinned node: not freed.
    assert!(!tree.free_detached_tree_if_unpinned(div));
    assert!(tree.get(span).is_some());

    tree.unpin(span);
    assert!(tree.free_detached_tree_if_unpinned(div));
    assert!(tree.get(span).is_none());
    assert!(tree.get(div).is_none());
    // Stale id: freeing again is a no-op.
    assert!(!tree.free_detached_tree_if_unpinned(div));
}

#[test]
fn document_tree_is_never_freed() {
    let mut tree = parse("<div></div>");
    let div = find_element(&tree, "div");
    assert!(!tree.free_detached_tree_if_unpinned(div));
    assert!(tree.get(div).is_some());
}

#[test]
fn free_subtree_refuses_pinned_nodes() {
    let mut tree = DomTree::new();
    let el = tree.create_element(oxidepage_dom::node::html_name("div".into()), Vec::new());
    tree.pin(el);
    assert!(tree.free_subtree(el).is_err());
    tree.unpin(el);
    assert!(tree.free_subtree(el).is_ok());
}

// === Cloning ===

#[test]
fn shallow_and_deep_clone() {
    let mut tree = parse(r#"<div id="a" class="x y"><span>text</span></div>"#);
    let div = find_element(&tree, "div");

    let shallow = tree.clone_subtree(div, false).unwrap();
    assert!(tree.node(shallow).parent().is_none());
    assert!(tree.node(shallow).first_child().is_none());
    let el = tree.node(shallow).as_element().unwrap();
    assert_eq!(el.id().map(|i| &**i), Some("a"));
    assert_eq!(el.classes().len(), 2);

    let deep = tree.clone_subtree(div, true).unwrap();
    assert!(tree.is_equal_node(div, deep));
    assert_eq!(tree.text_content(deep), "text");
}

#[test]
fn clone_document_is_not_supported() {
    let mut tree = parse("<p></p>");
    let doc = tree.document();
    assert!(tree.clone_subtree(doc, true).is_err());
}

#[test]
fn deep_clone_copies_template_contents() {
    let mut tree = parse("<template><b>inner</b></template>");
    let template = find_element(&tree, "template");
    let deep = tree.clone_subtree(template, true).unwrap();
    let contents = tree
        .node(deep)
        .as_element()
        .unwrap()
        .template_contents()
        .expect("cloned template has contents");
    assert_eq!(tree.text_content(contents), "inner");
}

/// Number of `<div>` elements reachable by following `first_child` from `root`
/// (inclusive of `root`).
fn div_chain_len(tree: &DomTree, root: oxidepage_dom::NodeId) -> usize {
    let mut count = 0;
    let mut node = Some(root);
    while let Some(id) = node {
        if tree
            .node(id)
            .as_element()
            .is_some_and(|el| &*el.name.local == "div")
        {
            count += 1;
        }
        node = tree.node(id).first_child();
    }
    count
}

/// Runs `body` on a thread with a deliberately small (128 KiB) stack. The
/// iterative traversals under test use O(1) native stack, so they complete
/// here; a recursive traversal of a `DEEP_NESTING`-deep tree would exhaust this
/// stack and abort, so any regression to recursion is caught. The tree is built
/// inside the thread because `DomTree` is not `Send`; only the plain result
/// crosses back out.
fn on_small_stack<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(body)
        .unwrap()
        .join()
        .unwrap()
}

/// Deep enough to overflow a 128 KiB stack many times over under recursion,
/// while still building quickly.
const DEEP_NESTING: usize = 3000;

/// Regression: `cloneNode(true)` on a deeply nested tree must not overflow the
/// stack (the traversal is iterative, not recursive).
#[test]
fn deep_clone_does_not_overflow_the_stack() {
    let (original_len, clone_len, clone_is_detached) = on_small_stack(|| {
        let html = "<div>".repeat(DEEP_NESTING);
        let mut tree = parse(&html);
        let root = find_element(&tree, "div");
        let original_len = div_chain_len(&tree, root);
        let clone = tree.clone_subtree(root, true).unwrap();
        (
            original_len,
            div_chain_len(&tree, clone),
            tree.node(clone).parent().is_none(),
        )
    });
    assert_eq!(original_len, DEEP_NESTING);
    assert_eq!(clone_len, DEEP_NESTING);
    assert!(clone_is_detached);
}

/// Regression: `innerHTML`/`outerHTML` on a deeply nested tree must not overflow
/// the stack (serialization is iterative, not recursive).
#[test]
fn deep_serialization_does_not_overflow_the_stack() {
    let (outer_divs, outer_closers, inner_divs) = on_small_stack(|| {
        let html = "<div>".repeat(DEEP_NESTING);
        let tree = parse(&html);
        let root = find_element(&tree, "div");
        let outer = outer_html(&tree, root);
        let inner = inner_html(&tree, root);
        (
            outer.matches("<div>").count(),
            outer.matches("</div>").count(),
            inner.matches("<div>").count(),
        )
    });
    // The root plus its nested descendants: `DEEP_NESTING` opening/closing tags.
    assert_eq!(outer_divs, DEEP_NESTING);
    assert_eq!(outer_closers, DEEP_NESTING);
    // innerHTML omits the root element.
    assert_eq!(inner_divs, DEEP_NESTING - 1);
}

// === isEqualNode / compareDocumentPosition ===

#[test]
fn is_equal_node_compares_structure() {
    let mut tree = parse(r#"<div class="a"><i>x</i></div>"#);
    let div = find_element(&tree, "div");
    let clone = tree.clone_subtree(div, true).unwrap();
    assert!(tree.is_equal_node(div, clone));

    // Different attribute value → not equal.
    let shallow = tree.clone_subtree(div, false).unwrap();
    tree.set_attribute(
        shallow,
        oxidepage_dom::node::attr_name("class".into()),
        "b".into(),
    );
    assert!(!tree.is_equal_node(div, shallow));
}

#[test]
fn compare_document_position_covers_all_cases() {
    let tree = parse("<div><span>a</span><b>b</b></div>");
    let div = find_element(&tree, "div");
    let span = find_element(&tree, "span");
    let b = find_element(&tree, "b");

    assert_eq!(tree.compare_document_position(div, div), 0);
    assert_eq!(
        tree.compare_document_position(span, div),
        DOCUMENT_POSITION_CONTAINS | DOCUMENT_POSITION_PRECEDING
    );
    assert_eq!(
        tree.compare_document_position(div, span),
        DOCUMENT_POSITION_CONTAINED_BY | DOCUMENT_POSITION_FOLLOWING
    );
    assert_eq!(
        tree.compare_document_position(span, b),
        DOCUMENT_POSITION_FOLLOWING
    );
    assert_eq!(
        tree.compare_document_position(b, span),
        DOCUMENT_POSITION_PRECEDING
    );
}

#[test]
fn compare_document_position_disconnected() {
    let mut tree = parse("<div></div>");
    let div = find_element(&tree, "div");
    let detached = tree.create_text("loose".into());
    let result = tree.compare_document_position(div, detached);
    assert_ne!(result & DOCUMENT_POSITION_DISCONNECTED, 0);
    // Consistent regardless of direction.
    let reverse = tree.compare_document_position(detached, div);
    assert_ne!(reverse & DOCUMENT_POSITION_DISCONNECTED, 0);
    let forward_order = result & (DOCUMENT_POSITION_PRECEDING | DOCUMENT_POSITION_FOLLOWING);
    let reverse_order = reverse & (DOCUMENT_POSITION_PRECEDING | DOCUMENT_POSITION_FOLLOWING);
    assert_ne!(forward_order, reverse_order);
}

// === normalize ===

#[test]
fn normalize_merges_text_runs() {
    let mut tree = parse("<div></div>");
    let div = find_element(&tree, "div");
    let t1 = tree.create_text("a".into());
    let t2 = tree.create_text("b".into());
    let empty = tree.create_text("".into());
    let t3 = tree.create_text("c".into());
    tree.append_child(div, t1).unwrap();
    tree.append_child(div, t2).unwrap();
    tree.append_child(div, empty).unwrap();
    tree.append_child(div, t3).unwrap();

    let detached = tree.normalize(div);
    assert_eq!(detached.len(), 3);
    assert_eq!(tree.children(div).count(), 1);
    assert_eq!(tree.text_content(div), "abc");
}

// === Document URL ===

#[test]
fn document_url_roundtrip() {
    let mut tree = DomTree::new();
    assert_eq!(tree.document_url(), "about:blank");
    tree.set_document_url("file:///tmp/x.html".into());
    assert_eq!(tree.document_url(), "file:///tmp/x.html");
}

// === Selectors ===

#[test]
fn query_selector_basics() {
    let tree = parse(
        r#"<div id="root" class="box">
             <p class="a">one</p>
             <p class="a b">two</p>
             <span data-x="1 2">three</span>
           </div>"#,
    );
    let doc = tree.document();

    let by_id = parse_selector_list("#root").unwrap();
    assert_eq!(
        tree.query_selector(doc, &by_id),
        Some(find_element(&tree, "div"))
    );

    let by_class = parse_selector_list("p.a").unwrap();
    assert_eq!(tree.query_selector_all(doc, &by_class).len(), 2);

    let compound = parse_selector_list(".a.b").unwrap();
    assert_eq!(tree.query_selector_all(doc, &compound).len(), 1);

    let attr = parse_selector_list(r#"span[data-x~="2"]"#).unwrap();
    assert_eq!(
        tree.query_selector(doc, &attr),
        Some(find_element(&tree, "span"))
    );

    let none = parse_selector_list(".missing").unwrap();
    assert_eq!(tree.query_selector(doc, &none), None);
}

#[test]
fn query_selector_combinators_and_structural_pseudos() {
    let tree = parse("<ul><li>1</li><li>2</li><li>3</li></ul>");
    let doc = tree.document();

    let child = parse_selector_list("ul > li:nth-child(2)").unwrap();
    let second = tree.query_selector(doc, &child).expect("matches");
    assert_eq!(tree.text_content(second), "2");

    let last = parse_selector_list("li:last-child").unwrap();
    let third = tree.query_selector(doc, &last).expect("matches");
    assert_eq!(tree.text_content(third), "3");

    let not = parse_selector_list("li:not(:first-child)").unwrap();
    assert_eq!(tree.query_selector_all(doc, &not).len(), 2);

    let is = parse_selector_list(":is(ol, ul) li").unwrap();
    assert_eq!(tree.query_selector_all(doc, &is).len(), 3);
}

#[test]
fn matches_and_closest() {
    let tree = parse(r#"<div class="outer"><p><b id="deep">x</b></p></div>"#);
    let b = find_element(&tree, "b");
    let div = find_element(&tree, "div");

    let sel = parse_selector_list("#deep").unwrap();
    assert!(tree.element_matches(b, &sel));

    let closest_sel = parse_selector_list("div.outer").unwrap();
    assert_eq!(tree.closest(b, &closest_sel), Some(div));
    // closest matches the element itself first.
    let self_sel = parse_selector_list("b").unwrap();
    assert_eq!(tree.closest(b, &self_sel), Some(b));
}

#[test]
fn interactive_pseudo_classes_parse_but_match_nothing() {
    // Phase 4 replaces the DOM-only SelectorImpl with stylo's, so `:hover`,
    // `:focus`, `::before`, … now parse (no SyntaxError) but match nothing
    // because the element state is empty until interactivity exists (P6).
    let tree = parse("<a href='#'>link</a><p>text</p>");
    let doc = tree.document();

    let hover = parse_selector_list(":hover").expect(":hover parses");
    assert_eq!(tree.query_selector(doc, &hover), None);
    let before = parse_selector_list("p::before").expect("::before parses");
    assert_eq!(tree.query_selector(doc, &before), None);

    // `:link` matches an `<a href>`, exercising the state-independent branch.
    let link = parse_selector_list(":link").expect(":link parses");
    assert!(tree.query_selector(doc, &link).is_some());

    // Genuinely malformed selectors are still SyntaxErrors.
    assert!(parse_selector_list("p..").is_err());
    assert!(parse_selector_list("!!!").is_err());
}

#[test]
fn type_selector_matches_html_namespace_only() {
    let tree = parse("<svg><rect/></svg><p>hi</p>");
    let doc = tree.document();
    let p = parse_selector_list("p").unwrap();
    assert!(tree.query_selector(doc, &p).is_some());
    // The SVG rect is in the SVG namespace but a bare type selector matches
    // any namespace (no default namespace declared).
    let rect = parse_selector_list("rect").unwrap();
    assert!(tree.query_selector(doc, &rect).is_some());
}

// === Template contents helper ===

#[test]
fn ensure_template_contents_is_idempotent() {
    let mut tree = DomTree::new();
    let template = tree.create_element(
        oxidepage_dom::node::html_name("template".into()),
        Vec::new(),
    );
    let contents = tree.ensure_template_contents(template);
    assert_eq!(tree.ensure_template_contents(template), contents);
    assert!(matches!(
        tree.node(contents).data(),
        NodeData::DocumentFragment { host: Some(h), .. } if *h == template
    ));
}
