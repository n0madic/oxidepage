//! DOM-side custom-element state machine and reaction-intent queue.

use html5ever::local_name;
use oxidepage_dom::custom_element::{CustomElementReaction, CustomElementState};
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

fn state(tree: &DomTree, node: NodeId) -> CustomElementState {
    tree.node(node).as_element().unwrap().custom_state()
}

fn ce_name(local: &str) -> html5ever::QualName {
    html5ever::QualName::new(None, html5ever::ns!(html), local.into())
}

#[test]
fn valid_custom_name_starts_undefined() {
    let mut tree = DomTree::new();
    let el = tree.create_element(ce_name("x-foo"), vec![]);
    assert_eq!(state(&tree, el), CustomElementState::Undefined);
}

#[test]
fn plain_element_is_uncustomized() {
    let mut tree = DomTree::new();
    let el = tree.create_element(html_name(local_name!("div")), vec![]);
    assert_eq!(state(&tree, el), CustomElementState::Uncustomized);
    // No reactions for plain elements.
    assert!(tree.take_custom_element_reactions().is_empty());
}

#[test]
fn define_before_create_enqueues_upgrade_on_create() {
    let mut tree = DomTree::new();
    tree.define_custom_element("x-foo".to_owned());
    let el = tree.create_element(ce_name("x-foo"), vec![]);
    let reactions = tree.take_custom_element_reactions();
    assert_eq!(reactions, vec![CustomElementReaction::Upgrade(el)]);
}

#[test]
fn late_define_upgrades_existing_element() {
    let mut tree = parse("<body><x-foo></x-foo></body>");
    let foo = find_element(&tree, "x-foo");
    assert_eq!(state(&tree, foo), CustomElementState::Undefined);
    // Nothing queued yet — no definition.
    assert!(tree.take_custom_element_reactions().is_empty());

    tree.define_custom_element("x-foo".to_owned());
    let reactions = tree.take_custom_element_reactions();
    assert_eq!(reactions, vec![CustomElementReaction::Upgrade(foo)]);
}

#[test]
fn connect_undefined_defined_enqueues_upgrade() {
    let mut tree = parse("<body></body>");
    let body = body(&tree);
    tree.define_custom_element("x-foo".to_owned());
    let foo = tree.create_element(ce_name("x-foo"), vec![]);
    // create enqueued an Upgrade already; drain it.
    let _ = tree.take_custom_element_reactions();
    // But foo is still Undefined until bindings upgrades it; simulate that the
    // reaction was consumed without upgrade would be wrong — instead test
    // connect of an Undefined-but-defined element enqueues upgrade.
    tree.append_child(body, foo).unwrap();
    let reactions = tree.take_custom_element_reactions();
    assert!(
        reactions.contains(&CustomElementReaction::Upgrade(foo)),
        "connect of a defined undefined element should enqueue upgrade, got {reactions:?}"
    );
}

#[test]
fn custom_element_connect_disconnect_reactions() {
    let mut tree = parse("<body></body>");
    let body = body(&tree);
    let foo = tree.create_element(ce_name("x-foo"), vec![]);
    // Simulate a successful upgrade (bindings sets this after running ctor).
    tree.set_custom_state(foo, CustomElementState::Custom);
    let _ = tree.take_custom_element_reactions();

    tree.append_child(body, foo).unwrap();
    let reactions = tree.take_custom_element_reactions();
    assert!(reactions.contains(&CustomElementReaction::Connected(foo)));

    tree.remove_child(body, foo).unwrap();
    let reactions = tree.take_custom_element_reactions();
    assert!(reactions.contains(&CustomElementReaction::Disconnected(foo)));
}

#[test]
fn attribute_change_only_for_custom_state() {
    let mut tree = parse("<body></body>");
    let body = body(&tree);
    let foo = tree.create_element(ce_name("x-foo"), vec![]);
    tree.append_child(body, foo).unwrap();
    let _ = tree.take_custom_element_reactions();

    // Undefined element: no attributeChanged reaction.
    tree.set_attribute(foo, attr_name(local_name!("title")), "a".into());
    let reactions = tree.take_custom_element_reactions();
    assert!(
        !reactions
            .iter()
            .any(|r| matches!(r, CustomElementReaction::AttributeChanged { .. })),
        "undefined element must not enqueue attributeChanged"
    );

    // After upgrade, attribute changes enqueue reactions.
    tree.set_custom_state(foo, CustomElementState::Custom);
    tree.set_attribute(foo, attr_name(local_name!("title")), "b".into());
    let reactions = tree.take_custom_element_reactions();
    assert_eq!(
        reactions,
        vec![CustomElementReaction::AttributeChanged {
            node: foo,
            name: "title".to_owned(),
            namespace: None,
            old: Some("a".to_owned()),
            new: Some("b".to_owned()),
        }]
    );
}

#[test]
fn clear_custom_elements_resets() {
    let mut tree = DomTree::new();
    tree.define_custom_element("x-foo".to_owned());
    assert!(tree.is_custom_element_defined("x-foo"));
    tree.clear_custom_elements();
    assert!(!tree.is_custom_element_defined("x-foo"));
    assert!(tree.take_custom_element_reactions().is_empty());
}
