//! Unit tests for the Phase 1 DOM core: spec mutation algorithms, mutation
//! observer record queuing, event dispatch, suspendable parsing, and
//! serialization.

use std::cell::RefCell;
use std::rc::Rc;

use html5ever::{local_name, ns};
use oxidepage_dom::node::{attr_name, html_name};
use oxidepage_dom::observer::ObserveInit;
use oxidepage_dom::serialize::{inner_html, outer_html};
use oxidepage_dom::{
    AddEventListenerOptions, Attribute, DomExceptionKind, DomTree, Event, LocalName,
    MutationRecordType, NodeData, NodeId, ParseOptions, ParseSignal, Parser, parse_document,
};

fn tree_with_body() -> (DomTree, NodeId, NodeId) {
    let mut tree = DomTree::new();
    let html = tree.create_element(html_name(local_name!("html")), vec![]);
    let body = tree.create_element(html_name(local_name!("body")), vec![]);
    let document = tree.document();
    tree.append_child(document, html).unwrap();
    tree.append_child(html, body).unwrap();
    (tree, html, body)
}

// === Mutation algorithms ===

#[test]
fn append_and_insert_before_maintain_sibling_order() {
    let (mut tree, _html, body) = tree_with_body();
    let a = tree.create_element(html_name(local_name!("div")), vec![]);
    let b = tree.create_element(html_name(local_name!("span")), vec![]);
    let c = tree.create_element(html_name(local_name!("p")), vec![]);
    tree.append_child(body, a).unwrap();
    tree.append_child(body, c).unwrap();
    tree.insert_before(body, b, Some(c)).unwrap();

    let children: Vec<NodeId> = tree.children(body).collect();
    assert_eq!(children, vec![a, b, c]);
    assert!(tree.node(a).is_connected());
    assert_eq!(tree.node(b).prev_sibling(), Some(a));
    assert_eq!(tree.node(b).next_sibling(), Some(c));
}

#[test]
fn remove_child_disconnects_subtree() {
    let (mut tree, _html, body) = tree_with_body();
    let div = tree.create_element(html_name(local_name!("div")), vec![]);
    let text = tree.create_text("hi".into());
    tree.append_child(body, div).unwrap();
    tree.append_child(div, text).unwrap();
    assert!(tree.node(text).is_connected());

    tree.remove_child(body, div).unwrap();
    assert!(!tree.node(div).is_connected());
    assert!(!tree.node(text).is_connected());
    assert_eq!(tree.node(div).parent(), None);
    // The detached subtree is still intact and re-insertable.
    tree.append_child(body, div).unwrap();
    assert!(tree.node(text).is_connected());
}

#[test]
fn moving_a_node_reparents_it() {
    let (mut tree, _html, body) = tree_with_body();
    let a = tree.create_element(html_name(local_name!("div")), vec![]);
    let b = tree.create_element(html_name(local_name!("section")), vec![]);
    let child = tree.create_element(html_name(local_name!("span")), vec![]);
    tree.append_child(body, a).unwrap();
    tree.append_child(body, b).unwrap();
    tree.append_child(a, child).unwrap();

    // Appending elsewhere implicitly removes from the old parent.
    tree.append_child(b, child).unwrap();
    assert_eq!(tree.node(child).parent(), Some(b));
    assert_eq!(tree.children(a).count(), 0);
}

#[test]
fn connected_scripts_are_queued_and_already_started_is_sticky() {
    let (mut tree, _html, body) = tree_with_body();
    let script = tree.create_element(html_name(local_name!("script")), vec![]);

    tree.append_child(body, script).unwrap();
    assert_eq!(tree.take_script_updates(), vec![script]);
    assert!(!tree.script_already_started(script));

    assert!(tree.mark_script_already_started(script));
    assert!(!tree.mark_script_already_started(script));
    tree.remove_child(body, script).unwrap();
    tree.append_child(body, script).unwrap();
    assert_eq!(tree.take_script_updates(), vec![script]);
    assert!(tree.script_already_started(script));
}

#[test]
fn fragment_insertion_moves_all_children() {
    let (mut tree, _html, body) = tree_with_body();
    let fragment = tree.create_document_fragment();
    let x = tree.create_element(html_name(local_name!("i")), vec![]);
    let y = tree.create_element(html_name(local_name!("b")), vec![]);
    tree.append_child(fragment, x).unwrap();
    tree.append_child(fragment, y).unwrap();

    tree.append_child(body, fragment).unwrap();
    let children: Vec<NodeId> = tree.children(body).collect();
    assert_eq!(children, vec![x, y]);
    assert_eq!(tree.children(fragment).count(), 0);
    assert!(tree.node(x).is_connected());
}

#[test]
fn replace_child_swaps_nodes() {
    let (mut tree, _html, body) = tree_with_body();
    let old = tree.create_element(html_name(local_name!("div")), vec![]);
    let new = tree.create_element(html_name(local_name!("span")), vec![]);
    tree.append_child(body, old).unwrap();

    let returned = tree.replace_child(body, new, old).unwrap();
    assert_eq!(returned, old);
    let children: Vec<NodeId> = tree.children(body).collect();
    assert_eq!(children, vec![new]);
    assert!(!tree.node(old).is_connected());
}

#[test]
fn inserting_ancestor_into_descendant_is_rejected() {
    let (mut tree, _html, body) = tree_with_body();
    let outer = tree.create_element(html_name(local_name!("div")), vec![]);
    let inner = tree.create_element(html_name(local_name!("div")), vec![]);
    tree.append_child(body, outer).unwrap();
    tree.append_child(outer, inner).unwrap();

    let err = tree.append_child(inner, outer).unwrap_err();
    assert_eq!(err.kind, DomExceptionKind::HierarchyRequestError);
}

#[test]
fn template_content_host_link_is_checked() {
    // Inserting a template element into its own contents fragment must be
    // rejected via the host-including inclusive ancestor check.
    let parsed = parse_document("<template><div></div></template>", ParseOptions::default());
    let mut tree = parsed.tree;
    let template = tree
        .inclusive_descendants(tree.document())
        .find(|&n| {
            tree.node(n)
                .as_element()
                .is_some_and(|el| el.name.local == local_name!("template"))
        })
        .expect("document contains a template");
    let contents = tree
        .node(template)
        .as_element()
        .unwrap()
        .template_contents()
        .unwrap();
    let err = tree.append_child(contents, template).unwrap_err();
    assert_eq!(err.kind, DomExceptionKind::HierarchyRequestError);
}

#[test]
fn document_constraints_are_enforced() {
    let mut tree = DomTree::new();
    let document = tree.document();
    let text = tree.create_text("nope".into());
    assert_eq!(
        tree.append_child(document, text).unwrap_err().kind,
        DomExceptionKind::HierarchyRequestError
    );

    let html = tree.create_element(html_name(local_name!("html")), vec![]);
    tree.append_child(document, html).unwrap();
    let second = tree.create_element(html_name(local_name!("div")), vec![]);
    assert_eq!(
        tree.append_child(document, second).unwrap_err().kind,
        DomExceptionKind::HierarchyRequestError
    );

    // A doctype may not be inserted after the document element.
    let doctype = tree.create_doctype("html".into(), "".into(), "".into());
    assert_eq!(
        tree.append_child(document, doctype).unwrap_err().kind,
        DomExceptionKind::HierarchyRequestError
    );
    // ... but is fine before it.
    tree.insert_before(document, doctype, Some(html)).unwrap();
}

#[test]
fn wrong_reference_child_is_not_found() {
    let (mut tree, _html, body) = tree_with_body();
    let a = tree.create_element(html_name(local_name!("div")), vec![]);
    let stranger = tree.create_element(html_name(local_name!("div")), vec![]);
    assert_eq!(
        tree.insert_before(body, a, Some(stranger))
            .unwrap_err()
            .kind,
        DomExceptionKind::NotFoundError
    );
}

#[test]
fn free_subtree_invalidates_ids() {
    let (mut tree, _html, body) = tree_with_body();
    let div = tree.create_element(html_name(local_name!("div")), vec![]);
    let text = tree.create_text("bye".into());
    tree.append_child(body, div).unwrap();
    tree.append_child(div, text).unwrap();

    // Connected subtrees cannot be freed.
    assert!(tree.free_subtree(div).is_err());
    tree.remove(div);
    tree.free_subtree(div).unwrap();
    assert!(tree.get(div).is_none());
    assert!(tree.get(text).is_none());
    assert!(tree.get(body).is_some());
}

// === Attributes and selector caches ===

#[test]
fn attribute_mutations_update_selector_caches() {
    let (mut tree, _html, body) = tree_with_body();
    tree.set_attribute(body, attr_name(local_name!("id")), "main".into());
    tree.set_attribute(body, attr_name(local_name!("class")), "a  b\tc".into());

    let el = tree.node(body).as_element().unwrap();
    assert_eq!(el.id().map(|a| &**a), Some("main"));
    let classes: Vec<&str> = el.classes().iter().map(|c| &**c).collect();
    assert_eq!(classes, vec!["a", "b", "c"]);

    tree.remove_attribute(body, &attr_name(local_name!("class")));
    assert!(tree.node(body).as_element().unwrap().classes().is_empty());
}

// === MutationObserver ===

#[test]
fn child_list_records_capture_siblings() {
    let (mut tree, _html, body) = tree_with_body();
    let observer = tree.observers_mut().create_observer();
    tree.observers_mut()
        .observe(
            observer,
            body,
            ObserveInit {
                child_list: true,
                ..ObserveInit::default()
            },
        )
        .unwrap();

    let a = tree.create_element(html_name(local_name!("div")), vec![]);
    let b = tree.create_element(html_name(local_name!("span")), vec![]);
    tree.append_child(body, a).unwrap();
    tree.append_child(body, b).unwrap();
    tree.remove_child(body, a).unwrap();

    let records = tree.observers_mut().take_records(observer);
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].record_type, MutationRecordType::ChildList);
    assert_eq!(records[0].added_nodes, vec![a]);
    assert_eq!(records[1].added_nodes, vec![b]);
    assert_eq!(records[1].previous_sibling, Some(a));
    assert_eq!(records[2].removed_nodes, vec![a]);
    assert_eq!(records[2].next_sibling, Some(b));
    // Queue drained.
    assert!(tree.observers_mut().take_records(observer).is_empty());
}

#[test]
fn subtree_and_old_value_options() {
    let (mut tree, html, body) = tree_with_body();
    let observer = tree.observers_mut().create_observer();
    tree.observers_mut()
        .observe(
            observer,
            html,
            ObserveInit {
                attributes: Some(true),
                attribute_old_value: Some(true),
                character_data: Some(true),
                character_data_old_value: Some(true),
                subtree: true,
                ..ObserveInit::default()
            },
        )
        .unwrap();

    tree.set_attribute(body, attr_name(local_name!("id")), "one".into());
    tree.set_attribute(body, attr_name(local_name!("id")), "two".into());
    let text = tree.create_text("first".into());
    tree.append_child(body, text).unwrap();
    tree.set_character_data(text, "second".into());

    let records = tree.observers_mut().take_records(observer);
    assert_eq!(records.len(), 3, "childList was not requested");
    assert_eq!(records[0].record_type, MutationRecordType::Attributes);
    assert_eq!(records[0].attribute_name, Some(local_name!("id")));
    assert_eq!(records[0].old_value, None);
    assert_eq!(records[1].old_value.as_deref(), Some("one"));
    assert_eq!(records[2].record_type, MutationRecordType::CharacterData);
    assert_eq!(records[2].old_value.as_deref(), Some("first"));
}

#[test]
fn attribute_filter_limits_records() {
    let (mut tree, _html, body) = tree_with_body();
    let observer = tree.observers_mut().create_observer();
    tree.observers_mut()
        .observe(
            observer,
            body,
            ObserveInit {
                attribute_filter: Some(vec![local_name!("id")]),
                ..ObserveInit::default()
            },
        )
        .unwrap();

    tree.set_attribute(body, attr_name(local_name!("id")), "x".into());
    tree.set_attribute(body, attr_name(local_name!("class")), "y".into());

    let records = tree.observers_mut().take_records(observer);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].attribute_name, Some(local_name!("id")));
}

#[test]
fn transient_observers_track_removed_subtrees() {
    let (mut tree, _html, body) = tree_with_body();
    let div = tree.create_element(html_name(local_name!("div")), vec![]);
    tree.append_child(body, div).unwrap();

    let observer = tree.observers_mut().create_observer();
    tree.observers_mut()
        .observe(
            observer,
            body,
            ObserveInit {
                attributes: Some(true),
                subtree: true,
                ..ObserveInit::default()
            },
        )
        .unwrap();

    tree.remove_child(body, div).unwrap();
    // div is detached, but mutations on it are still delivered to the
    // observer via its transient registration.
    tree.set_attribute(div, attr_name(local_name!("id")), "gone".into());
    let records = tree.observers_mut().take_records(observer);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type, MutationRecordType::Attributes);
    assert_eq!(records[0].target, div);

    // Per spec, `takeRecords()` does NOT clear transient registrations: further
    // same-task mutations on the removed subtree remain observed.
    tree.set_attribute(div, attr_name(local_name!("id")), "gone2".into());
    let records = tree.observers_mut().take_records(observer);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].target, div);

    // The microtask notify path (`take_records_for_notify`) is what clears the
    // transient registrations; afterwards the removed subtree is unobserved.
    tree.set_attribute(div, attr_name(local_name!("id")), "gone3".into());
    assert_eq!(
        tree.observers_mut().take_records_for_notify(observer).len(),
        1
    );
    tree.set_attribute(div, attr_name(local_name!("id")), "gone4".into());
    assert!(tree.observers_mut().take_records(observer).is_empty());
}

#[test]
fn observe_init_validation() {
    let err = ObserveInit::default().normalize().unwrap_err();
    assert!(err.0.contains("childList"));
    assert!(
        ObserveInit {
            attributes: Some(false),
            attribute_old_value: Some(true),
            ..ObserveInit::default()
        }
        .normalize()
        .is_err()
    );
    // attributeFilter implies attributes.
    let opts = ObserveInit {
        attribute_filter: Some(vec![local_name!("id")]),
        ..ObserveInit::default()
    }
    .normalize()
    .unwrap();
    assert!(opts.attributes);
}

// === Events ===

#[test]
fn dispatch_runs_capture_target_bubble_in_order() {
    let (mut tree, html, body) = tree_with_body();
    let div = tree.create_element(html_name(local_name!("div")), vec![]);
    tree.append_child(body, div).unwrap();

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
    let mk = |log: &Rc<RefCell<Vec<&'static str>>>, tag: &'static str| {
        let log = Rc::clone(log);
        Rc::new(move |_: &mut DomTree, _: &mut Event| log.borrow_mut().push(tag))
            as Rc<dyn Fn(&mut DomTree, &mut Event)>
    };

    let click = LocalName::from("click");
    tree.add_event_listener(
        html,
        click.clone(),
        mk(&log, "html-capture"),
        AddEventListenerOptions {
            capture: true,
            ..Default::default()
        },
    );
    tree.add_event_listener(
        html,
        click.clone(),
        mk(&log, "html-bubble"),
        Default::default(),
    );
    tree.add_event_listener(
        body,
        click.clone(),
        mk(&log, "body-bubble"),
        Default::default(),
    );
    tree.add_event_listener(div, click.clone(), mk(&log, "target"), Default::default());

    let mut event = Event::new(click, true, true);
    let not_canceled = tree.dispatch_event(div, &mut event).unwrap();
    assert!(not_canceled);
    assert_eq!(
        *log.borrow(),
        vec!["html-capture", "target", "body-bubble", "html-bubble"]
    );
    assert_eq!(event.target(), Some(div));
}

#[test]
fn remove_event_listener_reports_only_matching_id() {
    let (mut tree, _html, body) = tree_with_body();
    let noop =
        || Rc::new(|_: &mut DomTree, _: &mut Event| {}) as Rc<dyn Fn(&mut DomTree, &mut Event)>;
    let click = LocalName::from("click");

    // Two distinct listeners (distinct callbacks) on the same node.
    let a = tree.add_event_listener(body, click.clone(), noop(), Default::default());
    let b = tree.add_event_listener(body, click, noop(), Default::default());

    // Removing a live id removes exactly that entry and reports true.
    assert!(tree.remove_event_listener(body, a));
    // Removing it again reports false even though `b` still exists on the node
    // (the old code returned true whenever any listener remained) (L5).
    assert!(!tree.remove_event_listener(body, a));
    // `b` is untouched and still removable.
    assert!(tree.remove_event_listener(body, b));
    // With no listeners left, removal reports false.
    assert!(!tree.remove_event_listener(body, b));
}

#[test]
fn stop_propagation_and_prevent_default() {
    let (mut tree, _html, body) = tree_with_body();
    let div = tree.create_element(html_name(local_name!("div")), vec![]);
    tree.append_child(body, div).unwrap();

    let click = LocalName::from("click");
    tree.add_event_listener(
        div,
        click.clone(),
        Rc::new(|_: &mut DomTree, ev: &mut Event| {
            ev.prevent_default();
            ev.stop_propagation();
        }),
        Default::default(),
    );
    let bubbled: Rc<RefCell<bool>> = Rc::default();
    let bubbled2 = Rc::clone(&bubbled);
    tree.add_event_listener(
        body,
        click.clone(),
        Rc::new(move |_: &mut DomTree, _: &mut Event| *bubbled2.borrow_mut() = true),
        Default::default(),
    );

    let mut event = Event::new(click, true, true);
    let not_canceled = tree.dispatch_event(div, &mut event).unwrap();
    assert!(!not_canceled, "preventDefault must cancel the event");
    assert!(!*bubbled.borrow(), "stopPropagation must stop bubbling");
}

#[test]
fn once_listeners_fire_once_and_listeners_can_mutate() {
    let (mut tree, _html, body) = tree_with_body();
    let count: Rc<RefCell<u32>> = Rc::default();
    let count2 = Rc::clone(&count);
    let ping = LocalName::from("ping");
    tree.add_event_listener(
        body,
        ping.clone(),
        Rc::new(move |tree: &mut DomTree, ev: &mut Event| {
            *count2.borrow_mut() += 1;
            // Listeners may mutate the tree mid-dispatch.
            let target = ev.target().unwrap();
            let div = tree.create_element(html_name(local_name!("div")), vec![]);
            tree.append_child(target, div).unwrap();
        }),
        AddEventListenerOptions {
            once: true,
            ..Default::default()
        },
    );

    let mut e1 = Event::new(ping.clone(), false, false);
    tree.dispatch_event(body, &mut e1).unwrap();
    let mut e2 = Event::new(ping, false, false);
    tree.dispatch_event(body, &mut e2).unwrap();
    assert_eq!(*count.borrow(), 1);
    assert_eq!(tree.children(body).count(), 1);
}

// === Parser ===

#[test]
fn parses_a_simple_document() {
    let parsed = parse_document(
        "<!DOCTYPE html><title>t</title><p class=x>hello",
        ParseOptions::default(),
    );
    let dump = oxidepage_dom::dump::dump_document(&parsed.tree);
    let expected = "\
| <!DOCTYPE html>
| <html>
|   <head>
|     <title>
|       \"t\"
|   <body>
|     <p>
|       class=\"x\"
|       \"hello\"
";
    assert_eq!(dump, expected);
}

#[test]
fn parser_suspends_at_script_end_tags() {
    let mut parser = Parser::new_document(ParseOptions::default());
    parser.push_input("<body><script>var x = 1;</scr".into());
    assert_eq!(parser.run(), ParseSignal::InputExhausted);
    parser.push_input("ipt><div>after</div>".into());
    let signal = parser.run();
    let ParseSignal::Script(script) = signal else {
        panic!("expected a script suspension point, got {signal:?}");
    };

    // At the suspension point the script element is fully parsed...
    let parsed = {
        // ...and resuming afterwards parses the rest of the document.
        assert_eq!(parser.run(), ParseSignal::InputExhausted);
        parser.finish()
    };
    let el = parsed.tree.node(script).as_element().unwrap();
    assert_eq!(el.name.local, local_name!("script"));
    assert_eq!(parsed.tree.text_content(script), "var x = 1;");
    let body = parsed
        .tree
        .inclusive_descendants(parsed.tree.document())
        .find(|&n| {
            parsed
                .tree
                .node(n)
                .as_element()
                .is_some_and(|el| el.name.local == local_name!("body"))
        })
        .unwrap();
    assert_eq!(parsed.tree.children(body).count(), 2);
}

#[test]
fn noscript_parsing_follows_scripting_flag() {
    let html = "<head><noscript><style>p{}</style></noscript>";
    let scripting_on = parse_document(html, ParseOptions::default());
    let scripting_off = parse_document(
        html,
        ParseOptions {
            scripting_enabled: false,
            ..ParseOptions::default()
        },
    );
    // With scripting on, noscript contents parse as raw text; with
    // scripting off, they parse as markup.
    let on_dump = oxidepage_dom::dump::dump_document(&scripting_on.tree);
    let off_dump = oxidepage_dom::dump::dump_document(&scripting_off.tree);
    assert!(on_dump.contains("\"<style>p{}</style>\""), "{on_dump}");
    assert!(off_dump.contains("<style>"), "{off_dump}");
}

// === Serializer ===

#[test]
fn inner_and_outer_html_round_trip() {
    let parsed = parse_document(
        "<div id=a>x<b>y</b><br><img src='u'></div>",
        ParseOptions::default(),
    );
    let tree = &parsed.tree;
    let div = tree
        .inclusive_descendants(tree.document())
        .find(|&n| {
            tree.node(n)
                .as_element()
                .is_some_and(|el| el.name.local == local_name!("div"))
        })
        .unwrap();
    assert_eq!(inner_html(tree, div), "x<b>y</b><br><img src=\"u\">");
    assert_eq!(
        outer_html(tree, div),
        "<div id=\"a\">x<b>y</b><br><img src=\"u\"></div>"
    );
}

#[test]
fn template_serializes_its_contents() {
    let parsed = parse_document(
        "<template><p>inside</p></template>",
        ParseOptions::default(),
    );
    let tree = &parsed.tree;
    let template = tree
        .inclusive_descendants(tree.document())
        .find(|&n| {
            tree.node(n)
                .as_element()
                .is_some_and(|el| el.name.local == local_name!("template"))
        })
        .unwrap();
    assert_eq!(inner_html(tree, template), "<p>inside</p>");
}

#[test]
fn text_is_escaped_but_script_content_is_not() {
    let mut tree = DomTree::new();
    let html = tree.create_element(html_name(local_name!("html")), vec![]);
    let document = tree.document();
    tree.append_child(document, html).unwrap();
    let text = tree.create_text("a < b & c".into());
    tree.append_child(html, text).unwrap();
    assert_eq!(inner_html(&tree, html), "a &lt; b &amp; c");
}

// === Fragment parsing into an existing tree ===

#[test]
fn parse_fragment_into_grafts_nodes() {
    let (mut tree, _html, body) = tree_with_body();
    let owner = tree.document();
    let fragment = oxidepage_dom::parser::parse_fragment_into(
        &mut tree,
        "a<b>c</b>",
        html_name(local_name!("div")),
        ParseOptions::default(),
        owner,
    );
    tree.append_child(body, fragment).unwrap();
    assert_eq!(inner_html(&tree, body), "a<b>c</b>");
}

// === Attribute round-trip through the parser (sink add_attrs_if_missing) ===

#[test]
fn html_root_attrs_are_merged() {
    // The second <html> tag's attributes are added to the existing root.
    let parsed = parse_document(
        "<html lang=en><head></head><body><html dir=ltr lang=fr>",
        ParseOptions::default(),
    );
    let tree = &parsed.tree;
    let root = tree.document_element().unwrap();
    let el = tree.node(root).as_element().unwrap();
    assert_eq!(
        el.attr(&attr_name(local_name!("lang"))).map(|v| &**v),
        Some("en"),
        "existing attribute must not be overwritten"
    );
    assert_eq!(
        el.attr(&attr_name(local_name!("dir"))).map(|v| &**v),
        Some("ltr")
    );
}

#[test]
fn stale_node_ids_read_as_none() {
    let (mut tree, _html, body) = tree_with_body();
    let div = tree.create_element(html_name(local_name!("div")), vec![]);
    tree.append_child(body, div).unwrap();
    tree.remove(div);
    tree.free_subtree(div).unwrap();
    assert!(tree.get(div).is_none());

    // A new allocation may reuse the slot; the stale id must not alias it.
    let other = tree.create_element(html_name(local_name!("span")), vec![]);
    let _ = Attribute {
        name: attr_name(local_name!("id")),
        value: "fresh".into(),
    };
    assert_ne!(div, other);
    assert!(tree.get(div).is_none());
}

#[test]
fn document_kind_checks() {
    let tree = DomTree::new();
    assert!(matches!(
        tree.node(tree.document()).data(),
        NodeData::Document(_)
    ));
    assert_eq!(tree.node_count(), 1);
    let _ = ns!(html); // silence unused macro import when cfg tweaks tests
}
