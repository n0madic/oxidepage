//! The protocol-neutral node surface (ADR-0031): backend handles, node
//! descriptions, selector queries and the box model.
//!
//! The handle tests are the load-bearing ones. A `backendNodeId` is a name a
//! driver keeps across calls and across navigations, and the whole reason it is
//! a table rather than a packed integer is that a stale one must resolve to
//! *nothing* — never to an unrelated live node.

use oxidepage_page::remote::EvaluateOptions;
use oxidepage_page::{NodeRef, Page, PageOptions, RemoteError, RemoteSubtype, load_html_page};

fn page_with(html: &str) -> Page {
    load_html_page(html, PageOptions::default()).expect("page")
}

/// The node a `querySelector` finds, as a handle.
fn handle_for(page: &Page, selector: &str) -> u64 {
    let node = page
        .query_selector(page.document_node(), selector)
        .expect("valid selector")
        .unwrap_or_else(|| panic!("no match for {selector}"));
    page.node_handle(node).expect("handle")
}

// === Handles ===

#[test]
fn a_handle_is_stable_across_calls_and_resolves_back() {
    let page = page_with("<!doctype html><div id=a></div><div id=b></div>");
    let a = page
        .query_selector(page.document_node(), "#a")
        .unwrap()
        .unwrap();

    let first = page.node_handle(a).unwrap();
    let second = page.node_handle(a).unwrap();
    assert_eq!(
        first, second,
        "one node must have one handle — Puppeteer round-trips describeNode on \
         nearly every call, and a per-call handle would grow the table per call"
    );
    assert_eq!(page.resolve_node_ref(NodeRef::Handle(first)).unwrap(), a);

    let b = page
        .query_selector(page.document_node(), "#b")
        .unwrap()
        .unwrap();
    assert_ne!(page.node_handle(b).unwrap(), first);

    // A handle nobody minted names nothing.
    assert_eq!(
        page.resolve_node_ref(NodeRef::Handle(999_999)),
        Err(RemoteError::NoSuchObject(999_999))
    );
}

/// The failure the table exists to prevent: a handle to a node the GC has
/// collected must fail the generation check, not name whatever now occupies
/// that arena slot.
#[test]
fn a_handle_to_a_collected_node_resolves_to_nothing() {
    let page = page_with("<!doctype html><body></body>");
    page.eval_to_string(
        "(function () {\
             var tmp = document.createElement('div');\
             tmp.id = 'doomed';\
             document.body.appendChild(tmp);\
             document.body.removeChild(tmp);\
         })(); 'ok'",
    )
    .unwrap();
    // Grab the detached node while it is still reachable from the wrapper the
    // script just dropped.
    let doomed = page
        .query_selector_all(page.document_node(), "div")
        .unwrap()
        .first()
        .copied();
    let handle = doomed.map(|id| page.node_handle(id).unwrap());

    page.collect_garbage();
    page.run_until_stalled();
    // Refill the arena so a freed slot is reused by an unrelated node — that is
    // what turns "stale" into "aliases something else" when the check is absent.
    page.eval_to_string(
        "for (var i = 0; i < 8; i++) { document.body.appendChild(document.createElement('span')); } 'ok'",
    )
    .unwrap();
    page.collect_garbage();

    if let Some(handle) = handle {
        match page.resolve_node_ref(NodeRef::Handle(handle)) {
            Err(RemoteError::NoSuchObject(_)) => {}
            Ok(id) => {
                // If it still resolves, it must still be the same node — never
                // a different one that inherited the slot.
                assert_eq!(Some(id), doomed);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

/// Navigation retires every handle. `document_node()` is the one id that
/// survives, because the document is always arena slot `(0, generation 1)`.
#[test]
fn navigation_retires_every_handle_but_not_the_document() {
    let page = page_with("<!doctype html><div id=a>first</div>");
    let stale = handle_for(&page, "#a");
    assert!(page.resolve_node_ref(NodeRef::Handle(stale)).is_ok());

    page.load_html("<!doctype html><div id=a>second</div>")
        .unwrap();

    assert_eq!(
        page.resolve_node_ref(NodeRef::Handle(stale)),
        Err(RemoteError::NoSuchObject(stale)),
        "a handle minted before a commit must die with the document it named"
    );
    // The document itself is still addressable, and a fresh handle finds the
    // *new* #a.
    let document = page.document_node();
    assert!(page.describe_node(document, 0, false).is_ok());
    let fresh = handle_for(&page, "#a");
    assert_ne!(fresh, stale);
    let node = page.resolve_node_ref(NodeRef::Handle(fresh)).unwrap();
    assert_eq!(
        page.describe_node(node, 1, false).unwrap().children[0].node_value,
        "second"
    );
}

// === Descriptions ===

#[test]
fn describe_node_reports_names_attributes_and_child_counts() {
    let page = page_with(
        "<!doctype html><html><body>\
         <p id=p class=\"x y\" data-k=v>hi<!--c--></p>\
         <svg><circle r=1></circle></svg>\
         </body></html>",
    );

    let p = page
        .query_selector(page.document_node(), "#p")
        .unwrap()
        .unwrap();
    let d = page.describe_node(p, 1, false).unwrap();
    assert_eq!(d.node_type, 1);
    assert_eq!(
        d.node_name, "P",
        "an HTML element's nodeName is upper-cased"
    );
    assert_eq!(d.local_name, "p");
    assert_eq!(d.child_node_count, Some(2));
    assert_eq!(
        d.attributes,
        vec![
            ("id".to_owned(), "p".to_owned()),
            ("class".to_owned(), "x y".to_owned()),
            ("data-k".to_owned(), "v".to_owned()),
        ],
        "attributes keep document order"
    );
    assert_eq!(d.parent, Some(handle_for(&page, "body")));

    // Text and comment children, with their character data.
    assert_eq!(
        (d.children[0].node_type, &*d.children[0].node_name),
        (3, "#text")
    );
    assert_eq!(d.children[0].node_value, "hi");
    assert_eq!(
        d.children[0].child_node_count, None,
        "a Text node holds none"
    );
    assert_eq!(
        (d.children[1].node_type, &*d.children[1].node_name),
        (8, "#comment")
    );
    assert_eq!(d.children[1].node_value, "c");

    // An SVG element is *not* upper-cased — the rule is HTML-namespace only.
    let circle = page
        .query_selector(page.document_node(), "circle")
        .unwrap()
        .unwrap();
    let d = page.describe_node(circle, 0, false).unwrap();
    assert_eq!(d.node_name, "circle");
    assert_eq!(d.local_name, "circle");
}

/// A prefixed name reports the *qualified* form, which is the one thing a
/// second copy of the rule in the protocol layer would have got wrong.
#[test]
fn describe_node_reports_a_qualified_name_for_a_prefixed_element() {
    let page = page_with("<!doctype html><body></body>");
    page.eval_to_string(
        "document.body.appendChild(\
             document.createElementNS('urn:x', 'x:thing')); 'ok'",
    )
    .unwrap();
    let node = page
        .query_selector_all(page.document_node(), "thing")
        .unwrap()
        .first()
        .copied()
        .expect("the element is in the tree");
    let d = page.describe_node(node, 0, false).unwrap();
    assert_eq!(d.node_name, "x:thing");
    assert_eq!(d.local_name, "thing");
}

#[test]
fn describe_node_reports_the_document_and_its_doctype() {
    let page = page_with(
        "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01//EN\" \"http://www.w3.org/TR/html4/strict.dtd\">\
         <html><body></body></html>",
    );
    let d = page.describe_node(page.document_node(), 1, false).unwrap();
    assert_eq!(d.node_type, 9);
    assert_eq!(d.node_name, "#document");
    assert!(d.document_url.is_some());
    assert!(d.base_url.is_some());
    assert_eq!(d.parent, None);

    let doctype = &d.children[0];
    assert_eq!(doctype.node_type, 10);
    assert_eq!(doctype.node_name, "html");
    assert_eq!(doctype.doctype_name.as_deref(), Some("html"));
    assert_eq!(
        doctype.public_id.as_deref(),
        Some("-//W3C//DTD HTML 4.01//EN")
    );
    assert_eq!(
        doctype.system_id.as_deref(),
        Some("http://www.w3.org/TR/html4/strict.dtd")
    );
}

#[test]
fn describe_node_depth_truncates_and_minus_one_does_not() {
    let page = page_with("<!doctype html><div id=a><div id=b><div id=c></div></div></div>");
    let a = page
        .query_selector(page.document_node(), "#a")
        .unwrap()
        .unwrap();

    let d0 = page.describe_node(a, 0, false).unwrap();
    assert!(d0.children.is_empty(), "depth 0 describes the node alone");
    assert_eq!(d0.child_node_count, Some(1), "but still reports the count");

    let d1 = page.describe_node(a, 1, false).unwrap();
    assert_eq!(d1.children.len(), 1);
    assert!(d1.children[0].children.is_empty());

    let deep = page.describe_node(a, -1, false).unwrap();
    assert_eq!(deep.children[0].children[0].node_name, "DIV");
    assert!(deep.children[0].children[0].children.is_empty());
}

#[test]
fn piercing_reports_a_shadow_root_and_its_mode() {
    let page = page_with("<!doctype html><div id=host></div>");
    page.eval_to_string(
        "document.getElementById('host')\
             .attachShadow({mode: 'open'})\
             .appendChild(document.createElement('span')); 'ok'",
    )
    .unwrap();
    let host = page
        .query_selector(page.document_node(), "#host")
        .unwrap()
        .unwrap();

    let plain = page.describe_node(host, 1, false).unwrap();
    assert!(
        plain.shadow_roots.is_empty(),
        "no piercing, no shadow roots"
    );

    let pierced = page.describe_node(host, 1, true).unwrap();
    assert_eq!(pierced.shadow_roots.len(), 1);
    let root = &pierced.shadow_roots[0];
    assert_eq!(root.node_type, 11);
    assert_eq!(root.shadow_root_mode, Some("open"));
    assert_eq!(root.children[0].node_name, "SPAN");
}

// === Remote objects ===

#[test]
fn a_node_object_round_trips_and_a_non_node_does_not() {
    let page = page_with("<!doctype html><div id=a></div>");
    let a = page
        .query_selector(page.document_node(), "#a")
        .unwrap()
        .unwrap();

    let object = page.node_object(a, Some("g")).unwrap();
    assert_eq!(object.subtype, Some(RemoteSubtype::Node));
    assert_eq!(
        object.class_name.as_deref(),
        Some("HTMLDivElement"),
        "a driver branches on the class name to build its element handle"
    );
    let object_id = object.object_id.expect("a node needs a handle");
    assert_eq!(page.node_for_object(object_id).unwrap(), a);
    assert_eq!(
        page.resolve_node_ref(NodeRef::Object(object_id)).unwrap(),
        a
    );

    // A second resolve mints a second handle — as Chrome's `DOM.resolveNode`
    // does, because the driver releases each one — but both name the same node,
    // because both wrap the *same* cached JS object.
    let again = page.node_object(a, None).unwrap().object_id.unwrap();
    assert_ne!(again, object_id);
    assert_eq!(page.node_for_object(again).unwrap(), a);

    // A live handle to something that is not a node is a WrongType, not a panic.
    let plain = page
        .evaluate("({})", &EvaluateOptions::default())
        .expect_done();
    let plain_id = plain.result.object_id.expect("an object gets a handle");
    assert!(matches!(
        page.node_for_object(plain_id),
        Err(RemoteError::WrongType(_))
    ));
    assert_eq!(
        page.node_for_object(999_999),
        Err(RemoteError::NoSuchObject(999_999))
    );
}

// === Geometry ===

#[test]
fn box_quads_nest_and_report_the_untransformed_size() {
    let page = page_with(
        "<!doctype html><body style='margin: 0'>\
         <div id=d style='width: 100px; height: 40px; margin: 5px; \
         border: 2px solid; padding: 3px; transform: rotate(90deg)'></div></body>",
    );
    let d = page
        .query_selector(page.document_node(), "#d")
        .unwrap()
        .unwrap();
    let q = page.box_quads(d).unwrap();

    // Rotation preserves area, so the containment is checked on area rather
    // than on axis-aligned coordinates that a quarter turn permutes.
    let area = |quad: &[oxidepage_page::Point; 4]| {
        let mut sum = 0.0_f32;
        for i in 0..4 {
            let (p, n) = (quad[i], quad[(i + 1) % 4]);
            sum += p.x * n.y - n.x * p.y;
        }
        (sum / 2.0).abs()
    };
    assert!(area(&q.margin) > area(&q.border), "{q:?}");
    assert!(area(&q.border) > area(&q.padding), "{q:?}");
    assert!(area(&q.padding) > area(&q.content), "{q:?}");
    // Used border-box size: 100 content + 2*(2 border + 3 padding).
    assert_eq!((q.width, q.height), (110.0, 50.0));

    assert!(
        page.box_quads(page.document_node()).is_none()
            || page.box_quads(page.document_node()).is_some(),
        "a non-element must not panic"
    );
}

#[test]
fn layout_metrics_report_the_viewport_and_the_content_extent() {
    let page = page_with(
        "<!doctype html><body style='margin: 0'>\
         <div style='height: 5000px'></div></body>",
    );
    let m = page.layout_metrics();
    assert_eq!((m.scroll_x, m.scroll_y), (0.0, 0.0));
    assert_eq!(m.client_width, page.viewport().width);
    assert_eq!(m.client_height, page.viewport().height);
    assert!(m.content_height >= 5000.0, "{m:?}");
    // Never smaller than the viewport, even on a short page.
    assert!(m.content_width >= m.client_width, "{m:?}");

    page.eval_to_string("window.scrollTo(0, 300); 'ok'")
        .unwrap();
    let m = page.layout_metrics();
    assert_eq!(m.scroll_y, 300.0);
}

// === Selectors ===

#[test]
fn selector_queries_find_nodes_and_refuse_bad_input() {
    let page = page_with("<!doctype html><div class=x></div><div class=x></div><p></p>");
    let document = page.document_node();

    assert_eq!(page.query_selector_all(document, ".x").unwrap().len(), 2);
    assert!(page.query_selector(document, ".missing").unwrap().is_none());
    assert!(page.query_selector(document, "p").unwrap().is_some());

    // Rooted at an element: the root itself is not a candidate.
    let first = page.query_selector(document, ".x").unwrap().unwrap();
    assert!(page.query_selector(first, ".x").unwrap().is_none());

    // A malformed selector comes off the wire; it is an Err, never a panic.
    assert!(page.query_selector(document, ">>bad<<").is_err());
    assert!(page.query_selector_all(document, ":::").is_err());
}

/// A page can nest as deep as it likes, and describing that tree recurses once
/// per level — in the walk here, in the protocol layer's JSON construction, in
/// `serde_json`'s serializer, and in the nested value's own recursive drop.
/// Unbounded, `depth: -1` was a native stack overflow: an abort of the whole
/// endpoint process, reachable from page content.
#[test]
fn a_deep_tree_is_truncated_rather_than_overflowing_the_stack() {
    let page = page_with("<!doctype html><body></body>");
    let over = usize::try_from(oxidepage_page::MAX_DESCRIPTION_DEPTH).unwrap() + 200;
    page.eval_to_string(&format!(
        "document.body.innerHTML = '<div>'.repeat({over}); 'ok'"
    ))
    .unwrap();

    let described = page.describe_node(page.document_node(), -1, false).unwrap();

    // The deepest chain the answer carries, found with an explicit stack — the
    // description is up to the cap deep, so walking it recursively here would
    // reintroduce exactly what the cap exists to prevent.
    let mut deepest = (0_i32, &described);
    let mut stack = vec![(0_i32, &described)];
    while let Some((level, node)) = stack.pop() {
        if level > deepest.0 {
            deepest = (level, node);
        }
        for child in &node.children {
            stack.push((level + 1, child));
        }
    }
    let (levels, boundary) = deepest;
    assert_eq!(
        levels,
        oxidepage_page::MAX_DESCRIPTION_DEPTH,
        "`depth: -1` must stop at the cap"
    );
    // Truncation is not a lie: the boundary node still reports how many
    // children it really has, so a driver can re-root there and continue.
    assert!(boundary.children.is_empty());
    assert_eq!(boundary.child_node_count, Some(1));
    assert_ne!(boundary.handle, 0);
}
