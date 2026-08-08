//! Shadow DOM model tests (Phase 1): attachShadow validation, composed
//! connectedness, flat-tree children, slot assignment, id scoping.

use oxidepage_base::DomExceptionKind;
use oxidepage_dom::node::{attr_name, html_name};
use oxidepage_dom::{DomTree, NodeId, ParseOptions, ShadowMode, parse_document};

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

fn new_element(tree: &mut DomTree, local: &str) -> NodeId {
    tree.create_element(html_name(local.into()), Vec::new())
}

#[test]
fn attach_shadow_links_host_and_fragment() {
    let mut tree = parse("<div></div>");
    let div = find_element(&tree, "div");
    let sr = tree.attach_shadow(div, ShadowMode::Open).unwrap();
    assert_eq!(tree.shadow_root(div), Some(sr));
    assert_eq!(tree.shadow_host(sr), Some(div));
    assert_eq!(tree.shadow_mode(sr), Some(ShadowMode::Open));
    assert!(tree.is_shadow_root(sr));
    assert!(tree.has_shadow_roots());
}

#[test]
fn attach_shadow_twice_is_invalid_state() {
    let mut tree = parse("<div></div>");
    let div = find_element(&tree, "div");
    tree.attach_shadow(div, ShadowMode::Open).unwrap();
    let err = tree.attach_shadow(div, ShadowMode::Open).unwrap_err();
    assert_eq!(err.kind, DomExceptionKind::InvalidStateError);
}

#[test]
fn attach_shadow_rejects_invalid_host_names() {
    let mut tree = parse("<a></a>");
    let a = find_element(&tree, "a");
    let err = tree.attach_shadow(a, ShadowMode::Open).unwrap_err();
    assert_eq!(err.kind, DomExceptionKind::NotSupportedError);
    // Custom element names are valid hosts.
    let mut tree = DomTree::new();
    let custom = new_element(&mut tree, "swiper-container");
    assert!(tree.attach_shadow(custom, ShadowMode::Open).is_ok());
}

#[test]
fn shadow_tree_follows_host_connectedness() {
    let mut tree = parse("<div></div>");
    let div = find_element(&tree, "div");
    let sr = tree.attach_shadow(div, ShadowMode::Open).unwrap();
    // Host is connected, so the fragment is too.
    assert!(tree.node(sr).is_connected());
    let span = new_element(&mut tree, "span");
    tree.append_child(sr, span).unwrap();
    assert!(tree.node(span).is_connected());
    // Detaching the host disconnects the shadow tree.
    tree.remove(div);
    assert!(!tree.node(sr).is_connected());
    assert!(!tree.node(span).is_connected());
    // Re-attaching a subtree containing the host reconnects it.
    let body = find_element(&tree, "body");
    tree.append_child(body, div).unwrap();
    assert!(tree.node(sr).is_connected());
    assert!(tree.node(span).is_connected());
}

#[test]
fn attach_shadow_on_detached_host_stays_disconnected() {
    let mut tree = DomTree::new();
    let div = new_element(&mut tree, "div");
    let sr = tree.attach_shadow(div, ShadowMode::Closed).unwrap();
    assert!(!tree.node(sr).is_connected());
    assert_eq!(tree.shadow_mode(sr), Some(ShadowMode::Closed));
}

#[test]
fn flat_tree_children_projects_slots() {
    let mut tree =
        parse(r#"<div><span slot="a">named</span><b>default</b><i slot="missing">lost</i></div>"#);
    let div = find_element(&tree, "div");
    let named = find_element(&tree, "span");
    let default = find_element(&tree, "b");
    let lost = find_element(&tree, "i");

    let sr = tree.attach_shadow(div, ShadowMode::Open).unwrap();
    let wrapper = new_element(&mut tree, "section");
    tree.append_child(sr, wrapper).unwrap();
    let slot_a = new_element(&mut tree, "slot");
    tree.set_attribute(slot_a, attr_name("name".into()), "a".into());
    let slot_default = new_element(&mut tree, "slot");
    tree.append_child(wrapper, slot_a).unwrap();
    tree.append_child(wrapper, slot_default).unwrap();

    // Host's flat children are the shadow root's children.
    assert_eq!(tree.flat_tree_children(div), vec![wrapper]);
    // Named slot receives the matching light child.
    assert_eq!(tree.flat_tree_children(slot_a), vec![named]);
    // Default slot receives un-slotted children (text/elements without slot=).
    assert!(tree.flat_tree_children(slot_default).contains(&default));
    assert!(!tree.flat_tree_children(slot_default).contains(&lost));
    // An unmatched light child appears nowhere in the flat tree.
    assert_eq!(tree.assigned_slot(lost), None);
    assert_eq!(tree.flat_tree_parent(lost), None);
}

#[test]
fn slot_fallback_content_when_nothing_assigned() {
    let mut tree = parse("<div></div>");
    let div = find_element(&tree, "div");
    let sr = tree.attach_shadow(div, ShadowMode::Open).unwrap();
    let slot = new_element(&mut tree, "slot");
    tree.append_child(sr, slot).unwrap();
    let fallback = new_element(&mut tree, "em");
    tree.append_child(slot, fallback).unwrap();
    assert_eq!(tree.flat_tree_children(slot), vec![fallback]);
}

#[test]
fn text_nodes_assign_only_to_default_slot() {
    let mut tree = parse("<div>light text</div>");
    let div = find_element(&tree, "div");
    let text = tree.children(div).next().unwrap();
    let sr = tree.attach_shadow(div, ShadowMode::Open).unwrap();
    let named = new_element(&mut tree, "slot");
    tree.set_attribute(named, attr_name("name".into()), "x".into());
    let default = new_element(&mut tree, "slot");
    tree.append_child(sr, named).unwrap();
    tree.append_child(sr, default).unwrap();
    assert_eq!(tree.assigned_slot(text), Some(default));
    assert_eq!(tree.flat_tree_children(named), Vec::<NodeId>::new());
    assert_eq!(tree.flat_tree_children(default), vec![text]);
}

#[test]
fn first_slot_of_a_name_wins() {
    let mut tree = parse("<div><span>x</span></div>");
    let div = find_element(&tree, "div");
    let span = find_element(&tree, "span");
    let sr = tree.attach_shadow(div, ShadowMode::Open).unwrap();
    let first = new_element(&mut tree, "slot");
    let second = new_element(&mut tree, "slot");
    tree.append_child(sr, first).unwrap();
    tree.append_child(sr, second).unwrap();
    assert_eq!(tree.assigned_slot(span), Some(first));
    assert_eq!(tree.flat_tree_children(first), vec![span]);
    assert!(tree.flat_tree_children(second).is_empty());
}

#[test]
fn shadow_ids_do_not_leak_into_document_index() {
    let mut tree = parse("<div></div>");
    let div = find_element(&tree, "div");
    let sr = tree.attach_shadow(div, ShadowMode::Open).unwrap();
    let inner = new_element(&mut tree, "span");
    tree.set_attribute(inner, attr_name("id".into()), "scoped".into());
    tree.append_child(sr, inner).unwrap();
    assert!(tree.node(inner).is_connected());
    assert_eq!(tree.element_by_id(tree.document(), "scoped"), None);
    // Setting the id on an already-connected shadow element must not leak
    // either (reindex_id path).
    tree.set_attribute(inner, attr_name("id".into()), "renamed".into());
    assert_eq!(tree.element_by_id(tree.document(), "renamed"), None);
}

#[test]
fn containing_shadow_root_walks_to_fragment() {
    let mut tree = parse("<div><span>light</span></div>");
    let div = find_element(&tree, "div");
    let span = find_element(&tree, "span");
    let sr = tree.attach_shadow(div, ShadowMode::Open).unwrap();
    let inner = new_element(&mut tree, "p");
    tree.append_child(sr, inner).unwrap();
    assert_eq!(tree.containing_shadow_root(inner), Some(sr));
    assert_eq!(tree.containing_shadow_root(sr), Some(sr));
    assert_eq!(tree.containing_shadow_root(span), None);
    assert_eq!(tree.containing_shadow_root(div), None);
}

#[test]
fn structure_version_moves_on_attach_shadow() {
    let mut tree = parse("<div></div>");
    let div = find_element(&tree, "div");
    let before = tree.structure_version();
    tree.attach_shadow(div, ShadowMode::Open).unwrap();
    assert!(tree.structure_version() > before);
}

#[test]
fn freeing_host_subtree_frees_shadow_tree() {
    let mut tree = parse("<div></div>");
    let div = find_element(&tree, "div");
    let sr = tree.attach_shadow(div, ShadowMode::Open).unwrap();
    let inner = new_element(&mut tree, "span");
    tree.append_child(sr, inner).unwrap();
    tree.remove(div);
    assert!(tree.free_detached_tree_if_unpinned(div));
    assert!(tree.get(sr).is_none());
    assert!(tree.get(inner).is_none());
    assert!(!tree.has_shadow_roots());
}
