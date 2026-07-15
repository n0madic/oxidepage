//! The `DomTree` `id` index: it tracks exactly the connected elements carrying
//! an `id`, and `element_by_id` resolves duplicates in tree order.

use html5ever::local_name;
use oxidepage_dom::node::{attr_name, html_name};
use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};

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

fn body(tree: &DomTree) -> NodeId {
    find_element(tree, "body")
}

/// A detached `<div id="…">`.
fn new_div(tree: &mut DomTree, id: &str) -> NodeId {
    let div = tree.create_element(html_name(local_name!("div")), vec![]);
    tree.set_attribute(div, attr_name(local_name!("id")), id.into());
    div
}

#[test]
fn parsed_document_is_indexed() {
    let tree = parse("<div id='a'><span id='b'>hi</span></div>");
    assert_eq!(tree.element_by_id("a"), Some(find_element(&tree, "div")));
    assert_eq!(tree.element_by_id("b"), Some(find_element(&tree, "span")));
    assert_eq!(tree.element_by_id("missing"), None);

    let mut names: Vec<&str> = tree.id_names().collect();
    names.sort_unstable();
    assert_eq!(names, ["a", "b"]);
}

#[test]
fn detached_element_is_indexed_only_while_connected() {
    let mut tree = parse("<body></body>");
    let body = body(&tree);

    let div = new_div(&mut tree, "x");
    assert_eq!(
        tree.element_by_id("x"),
        None,
        "detached must not be indexed"
    );

    tree.append_child(body, div).unwrap();
    assert_eq!(tree.element_by_id("x"), Some(div));

    tree.remove_child(body, div).unwrap();
    assert_eq!(tree.element_by_id("x"), None);
}

#[test]
fn descendants_of_an_inserted_subtree_are_indexed() {
    let mut tree = parse("<body></body>");
    let body = body(&tree);
    let outer = new_div(&mut tree, "outer");
    let inner = new_div(&mut tree, "inner");
    tree.append_child(outer, inner).unwrap();
    assert_eq!(tree.element_by_id("inner"), None);

    tree.append_child(body, outer).unwrap();
    assert_eq!(tree.element_by_id("inner"), Some(inner));

    tree.remove_child(body, outer).unwrap();
    assert_eq!(tree.element_by_id("inner"), None);
}

#[test]
fn changing_and_removing_the_id_attribute_updates_the_index() {
    let mut tree = parse("<div id='old'></div>");
    let div = find_element(&tree, "div");

    tree.set_attribute(div, attr_name(local_name!("id")), "new".into());
    assert_eq!(tree.element_by_id("old"), None);
    assert_eq!(tree.element_by_id("new"), Some(div));

    tree.remove_attribute(div, &attr_name(local_name!("id")));
    assert_eq!(tree.element_by_id("new"), None);
    assert_eq!(tree.id_names().count(), 0);
}

#[test]
fn moving_a_node_between_parents_keeps_it_findable() {
    let mut tree = parse("<div id='a'></div><div id='b'></div>");
    let a = tree.element_by_id("a").unwrap();
    let b = tree.element_by_id("b").unwrap();
    let child = new_div(&mut tree, "child");
    tree.append_child(a, child).unwrap();
    assert_eq!(tree.element_by_id("child"), Some(child));

    // `append_child` on an attached node removes it from its old parent first.
    tree.append_child(b, child).unwrap();
    assert_eq!(tree.node(child).parent(), Some(b));
    assert_eq!(tree.element_by_id("child"), Some(child));
}

#[test]
fn clone_is_not_indexed_until_inserted() {
    let mut tree = parse("<body><div id='a'></div></body>");
    let body = body(&tree);
    let original = tree.element_by_id("a").unwrap();

    let copy = tree.clone_subtree(original, true).unwrap();
    assert_eq!(
        tree.element_by_id("a"),
        Some(original),
        "the detached clone must not displace the original"
    );

    tree.append_child(body, copy).unwrap();
    assert_eq!(
        tree.element_by_id("a"),
        Some(original),
        "duplicate ids resolve to the first element in tree order"
    );

    tree.remove_child(body, original).unwrap();
    assert_eq!(tree.element_by_id("a"), Some(copy));
}

#[test]
fn duplicate_ids_resolve_in_tree_order() {
    let mut tree = parse("<body><span id='dup'></span></body>");
    let body = body(&tree);
    let first = tree.element_by_id("dup").unwrap();

    // Insert a second `dup` *before* the first: tree order, not insertion order.
    let earlier = new_div(&mut tree, "dup");
    tree.insert_before(body, earlier, Some(first)).unwrap();
    assert_eq!(tree.element_by_id("dup"), Some(earlier));

    tree.remove_child(body, earlier).unwrap();
    assert_eq!(tree.element_by_id("dup"), Some(first));
}

#[test]
fn id_version_moves_on_index_changes_only() {
    let mut tree = parse("<body></body>");
    let body = body(&tree);
    let mut version = tree.id_version();

    fn assert_bumped(tree: &DomTree, version: &mut u64, what: &str) {
        assert!(
            tree.id_version() > *version,
            "id_version stalled after {what}"
        );
        *version = tree.id_version();
    }

    let div = tree.create_element(html_name(local_name!("div")), vec![]);
    tree.append_child(body, div).unwrap();
    assert_eq!(
        tree.id_version(),
        version,
        "an id-less insert must not bump"
    );

    tree.set_attribute(div, attr_name(local_name!("id")), "a".into());
    assert_bumped(&tree, &mut version, "set_attribute(id)");

    tree.set_attribute(div, attr_name(local_name!("id")), "b".into());
    assert_bumped(&tree, &mut version, "changing the id");

    tree.remove_child(body, div).unwrap();
    assert_bumped(&tree, &mut version, "disconnecting an indexed element");

    tree.append_child(body, div).unwrap();
    assert_bumped(&tree, &mut version, "reconnecting an indexed element");

    tree.remove_attribute(div, &attr_name(local_name!("id")));
    assert_bumped(&tree, &mut version, "remove_attribute(id)");

    // Mutations that cannot touch the index leave the version alone.
    tree.set_attribute(div, attr_name(local_name!("class")), "c".into());
    tree.set_attribute(div, attr_name(local_name!("title")), "t".into());
    let text = tree.create_text("hello".into());
    tree.append_child(div, text).unwrap();
    assert_eq!(
        tree.id_version(),
        version,
        "unrelated mutations must not bump"
    );

    // Nor does an id write on a detached element.
    let detached = new_div(&mut tree, "z");
    assert_eq!(tree.id_version(), version);
    assert_eq!(tree.element_by_id("z"), None);
    let _ = detached;
}
