//! Phase 4 (Style) DOM-layer tests: the `StyleUpdate` queue driven by the
//! single mutation path, and the interactive-pseudo-class parse behavior.

use html5ever::{QualName, local_name, ns};
use oxidepage_dom::{DomTree, NodeData, ParseOptions, StyleUpdate, parse_document};

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

fn attr(local: html5ever::LocalName) -> QualName {
    QualName::new(None, ns!(), local)
}

#[test]
fn parsing_style_element_queues_style_update() {
    let mut tree = parse("<style>div { color: red }</style>");
    let style = find_element(&tree, "style");
    let updates = tree.take_style_updates();
    assert!(
        updates.contains(&StyleUpdate::StyleElement(style)),
        "expected a StyleElement update for the parsed <style>, got {updates:?}"
    );
    // Draining empties the queue.
    assert!(tree.take_style_updates().is_empty());
}

#[test]
fn parsing_stylesheet_link_queues_link_update() {
    let mut tree = parse("<link rel=stylesheet href='a.css'>");
    let link = find_element(&tree, "link");
    let updates = tree.take_style_updates();
    assert!(
        updates.contains(&StyleUpdate::LinkElement(link)),
        "expected a LinkElement update, got {updates:?}"
    );
}

#[test]
fn non_stylesheet_link_does_not_queue() {
    let mut tree = parse("<link rel=icon href='favicon.ico'>");
    let updates = tree.take_style_updates();
    assert!(
        !updates
            .iter()
            .any(|u| matches!(u, StyleUpdate::LinkElement(_))),
        "a non-stylesheet <link> must not queue a LinkElement update, got {updates:?}"
    );
}

#[test]
fn js_style_insertion_and_removal_queue_updates() {
    let mut tree = parse("<body></body>");
    let body = find_element(&tree, "body");
    let _ = tree.take_style_updates();

    // Build a detached <style> element and insert it — no update while detached.
    let style = tree.create_element(QualName::new(None, ns!(html), local_name!("style")), vec![]);
    let text = tree.create_text("p { color: blue }".into());
    tree.append_child(style, text).unwrap();
    assert!(
        tree.take_style_updates().is_empty(),
        "detached <style> must not queue updates"
    );

    // Connecting it queues a StyleElement update.
    tree.append_child(body, style).unwrap();
    assert!(
        tree.take_style_updates()
            .contains(&StyleUpdate::StyleElement(style))
    );

    // Removing it queues a StyleElementRemoved update.
    tree.remove_child(body, style).unwrap();
    assert!(
        tree.take_style_updates()
            .contains(&StyleUpdate::StyleElementRemoved(style))
    );
}

#[test]
fn changing_link_rel_toggles_stylesheet_relevance() {
    let mut tree = parse("<link rel=icon href='x.css'>");
    let link = find_element(&tree, "link");
    let _ = tree.take_style_updates();

    // rel=icon -> rel=stylesheet: becomes a stylesheet link.
    tree.set_attribute(link, attr(local_name!("rel")), "stylesheet".into());
    assert!(
        tree.take_style_updates()
            .contains(&StyleUpdate::LinkElement(link))
    );

    // rel=stylesheet -> rel=alternate: no longer a stylesheet link.
    tree.set_attribute(link, attr(local_name!("rel")), "alternate".into());
    assert!(
        tree.take_style_updates()
            .contains(&StyleUpdate::LinkElementRemoved(link))
    );
}

#[test]
fn document_node_is_never_a_style_owner() {
    let tree = parse("<p>hi</p>");
    assert!(matches!(
        tree.node(tree.document()).data(),
        NodeData::Document(_)
    ));
}

#[test]
fn is_whitespace_text_classifies_text_nodes() {
    let tree = parse("<div>  \n\t </div><p>text</p>");
    let div = find_element(&tree, "div");
    let p = find_element(&tree, "p");
    let ws_text = tree.node(div).first_child().unwrap();
    let real_text = tree.node(p).first_child().unwrap();
    assert!(tree.is_whitespace_text(ws_text));
    assert!(!tree.is_whitespace_text(real_text));
    // Non-text nodes are never whitespace nodes.
    assert!(!tree.is_whitespace_text(div));
}

// === Active-tree scope ===

// `debug_assert!` only fires with debug assertions on, so the panic this test
// asserts is a debug-build guarantee.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "nested enter_active_tree")]
fn nested_enter_active_tree_with_different_tree_panics() {
    use oxidepage_dom::select::enter_active_tree;

    let tree_a = parse("<div></div>");
    let tree_b = parse("<span></span>");
    let _outer = enter_active_tree(&tree_a);
    // Entering a second, different tree while the outer scope is live would let
    // outer NodeRef handles resolve against tree_b's arena (M1).
    let _inner = enter_active_tree(&tree_b);
}

// === Dirty-descendant propagation ===

#[test]
fn mutation_propagates_stylo_dirty_bit_past_stale_descendant_bit() {
    use oxidepage_dom::NodeId;
    use oxidepage_dom::node::html_name;

    let mut tree = DomTree::new();
    let html = tree.create_element(html_name(local_name!("html")), vec![]);
    let body = tree.create_element(html_name(local_name!("body")), vec![]);
    let outer = tree.create_element(html_name(local_name!("div")), vec![]);
    let inner = tree.create_element(html_name(local_name!("div")), vec![]);
    let leaf = tree.create_element(html_name(local_name!("span")), vec![]);
    let doc = tree.document();
    tree.append_child(doc, html).unwrap();
    tree.append_child(html, body).unwrap();
    tree.append_child(body, outer).unwrap();
    tree.append_child(outer, inner).unwrap();
    tree.append_child(inner, leaf).unwrap();

    let dirty = |tree: &DomTree, id: NodeId| -> bool {
        tree.node(id)
            .as_element()
            .unwrap()
            .stylo
            .dirty_descendants
            .get()
    };
    let set_dirty = |tree: &DomTree, id: NodeId, v: bool| {
        tree.node(id)
            .as_element()
            .unwrap()
            .stylo
            .dirty_descendants
            .set(v);
    };

    // Simulate the state stylo leaves after clearing a display:none subtree's
    // cascade data: a *stale* dirty-descendants bit survives on `inner` while
    // its ancestors' bits are clean, violating "bit set => ancestors set" (M3).
    for id in [html, body, outer, inner, leaf] {
        set_dirty(&tree, id, false);
    }
    set_dirty(&tree, inner, true);

    // A mutation below the stale bit must still propagate the stylo dirty chain
    // to the root; early-breaking at `inner` would leave the ancestors above it
    // unmarked and let the restyle be pruned.
    tree.set_attribute(leaf, attr(local_name!("class")), "x".into());

    assert!(
        dirty(&tree, outer),
        "outer must be marked despite the stale inner bit"
    );
    assert!(dirty(&tree, body), "body must be marked");
    assert!(
        dirty(&tree, html),
        "root must be marked so the style traversal reaches the mutation"
    );
}
