//! The `DOM` domain over the wire.
//!
//! The load-bearing case is the `describeNode` + `resolveNode` round trip:
//! nearly every Puppeteer `ElementHandle` method carries the
//! `bindIsolatedHandle` decorator, which converts a handle to a
//! `backendNodeId` and back on every call. If that loses node identity, every
//! query, click and `$eval` fails.

mod common;

use common::{Client, Fixtures, Harness};
use serde_json::{Value, json};

const PAGE: &str = "<!doctype html><html><head><title>t</title><style>\
    body { margin: 0 }\
    #heading { position: absolute; left: 10px; top: 20px; width: 100px; height: 40px;\
               margin: 5px; border: 2px solid; padding: 3px }\
    </style></head><body>\
    <h1 id=heading class=\"a b\" data-k=v>hello</h1>\
    <p class=item>one</p><p class=item>two</p>\
    </body></html>";

fn started() -> (Harness, Fixtures) {
    let fixtures = Fixtures::start(vec![("/", PAGE), ("/next", "<!doctype html><p>next</p>")]);
    (Harness::start(), fixtures)
}

/// A page loaded, with `DOM` enabled and the initial `frameNavigated` drained.
fn loaded(harness: &Harness, fixtures: &Fixtures) -> (Client, String, String) {
    let (mut client, session, target) = harness.attached();
    client.call_on(&session, "DOM.enable", json!({}));
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/") }),
    );
    client.await_event("Page.frameNavigated");
    (client, session, target)
}

fn document_id(client: &mut Client, session: &str) -> i64 {
    client.call_on(session, "DOM.getDocument", json!({}))["root"]["nodeId"]
        .as_i64()
        .expect("nodeId")
}

fn heading_id(client: &mut Client, session: &str) -> i64 {
    let root = document_id(client, session);
    client.call_on(
        session,
        "DOM.querySelector",
        json!({ "nodeId": root, "selector": "#heading" }),
    )["nodeId"]
        .as_i64()
        .expect("nodeId")
}

/// Puppeteer's `bindIsolatedHandle` round trip, end to end: an object handle
/// becomes a `backendNodeId`, comes back as an object handle, and still names
/// the same element.
#[test]
fn describe_node_and_resolve_node_round_trip_an_element() {
    let (harness, fixtures) = started();
    let (mut client, session, _target) = loaded(&harness, &fixtures);

    let found = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "document.querySelector('#heading')" }),
    );
    assert_eq!(found["result"]["subtype"], "node");
    assert_eq!(
        found["result"]["className"], "HTMLHeadingElement",
        "a driver branches on subtype and className to build its element handle"
    );
    let object_id = found["result"]["objectId"].as_str().unwrap().to_owned();

    let described = client.call_on(
        &session,
        "DOM.describeNode",
        json!({ "objectId": &object_id }),
    );
    let node = &described["node"];
    assert_eq!(node["nodeType"], 1);
    assert_eq!(node["nodeName"], "H1");
    assert_eq!(node["localName"], "h1");
    // One id space: the two members are deliberately the same number.
    assert_eq!(node["nodeId"], node["backendNodeId"]);
    // A flat alternating array, not a map and not a list of pairs.
    assert_eq!(
        node["attributes"],
        json!(["id", "heading", "class", "a b", "data-k", "v"])
    );
    // `frameId` must never appear: `contentFrame()` returns null iff it is not
    // a string, and null is the right answer until iframes exist.
    assert!(node.get("frameId").is_none(), "{node}");

    let backend = node["backendNodeId"].as_i64().unwrap();
    let resolved = client.call_on(
        &session,
        "DOM.resolveNode",
        json!({ "backendNodeId": backend }),
    );
    assert_eq!(resolved["object"]["subtype"], "node");
    let new_id = resolved["object"]["objectId"].as_str().unwrap().to_owned();

    // The handle really is the same element, not merely the same shape.
    let same = client.call_on(
        &session,
        "Runtime.callFunctionOn",
        json!({
            "functionDeclaration": "function () { return this.id + ':' + this.textContent; }",
            "objectId": new_id,
            "returnByValue": true,
        }),
    );
    assert_eq!(same["result"]["value"], "heading:hello");
}

#[test]
fn get_document_reports_the_tree_and_query_selector_finds_nodes() {
    let (harness, fixtures) = started();
    let (mut client, session, _target) = loaded(&harness, &fixtures);

    let root = client.call_on(&session, "DOM.getDocument", json!({ "depth": -1 }))["root"].clone();
    assert_eq!(root["nodeType"], 9);
    assert_eq!(root["nodeName"], "#document");
    assert!(root["documentURL"].as_str().unwrap().ends_with("/"));
    assert!(root["baseURL"].is_string());
    // A Text node holds no children, so Chrome omits the member entirely.
    let text = find_by_name(&root, "#text").expect("a text node somewhere in the tree");
    assert!(text.get("childNodeCount").is_none(), "{text}");

    let root_id = root["nodeId"].as_i64().unwrap();
    let all = client.call_on(
        &session,
        "DOM.querySelectorAll",
        json!({ "nodeId": root_id, "selector": ".item" }),
    );
    assert_eq!(all["nodeIds"].as_array().unwrap().len(), 2);

    // Chrome answers `0` for "no match", which is why handles start at 1.
    let miss = client.call_on(
        &session,
        "DOM.querySelector",
        json!({ "nodeId": root_id, "selector": ".nope" }),
    );
    assert_eq!(miss["nodeId"], 0);

    // A selector that does not parse is a refusal, not a panic.
    let error = client
        .try_call_on(
            &session,
            "DOM.querySelector",
            json!({ "nodeId": root_id, "selector": ":::" }),
        )
        .expect_err("a malformed selector must be refused");
    assert_eq!(error["code"], -32000, "{error}");
}

/// `requestNode` is the other direction of the same bridge.
#[test]
fn request_node_turns_an_object_handle_into_a_node_id() {
    let (harness, fixtures) = started();
    let (mut client, session, _target) = loaded(&harness, &fixtures);

    let object_id = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "document.querySelector('#heading')" }),
    )["result"]["objectId"]
        .as_str()
        .unwrap()
        .to_owned();

    let requested = client.call_on(
        &session,
        "DOM.requestNode",
        json!({ "objectId": object_id }),
    );
    assert_eq!(requested["nodeId"], heading_id(&mut client, &session));

    // A handle to something that is not a node is refused, not answered.
    let plain = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "({ not: 'a node' })" }),
    )["result"]["objectId"]
        .as_str()
        .unwrap()
        .to_owned();
    let error = client
        .try_call_on(&session, "DOM.requestNode", json!({ "objectId": plain }))
        .expect_err("a non-node handle must be refused");
    assert_eq!(error["code"], -32000, "{error}");
}

/// All three id forms drive the geometry commands, and the box model nests.
#[test]
fn each_id_form_drives_the_geometry_commands() {
    let (harness, fixtures) = started();
    let (mut client, session, _target) = loaded(&harness, &fixtures);

    let node_id = heading_id(&mut client, &session);
    let object_id = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "document.querySelector('#heading')" }),
    )["result"]["objectId"]
        .as_str()
        .unwrap()
        .to_owned();

    for target in [
        json!({ "nodeId": node_id }),
        json!({ "backendNodeId": node_id }),
        json!({ "objectId": &object_id }),
    ] {
        let model = client.call_on(&session, "DOM.getBoxModel", target.clone())["model"].clone();
        // Used border-box size: 100 content + 2*(2 border + 3 padding).
        assert_eq!(model["width"], 110.0, "{target}");
        assert_eq!(model["height"], 50.0, "{target}");
        // Eight numbers per quad, and the boxes nest outward.
        for name in ["content", "padding", "border", "margin"] {
            assert_eq!(model[name].as_array().unwrap().len(), 8, "{name}");
        }
        assert!(model["margin"][0].as_f64().unwrap() < model["border"][0].as_f64().unwrap());
        assert!(model["border"][0].as_f64().unwrap() < model["padding"][0].as_f64().unwrap());
        assert!(model["padding"][0].as_f64().unwrap() < model["content"][0].as_f64().unwrap());

        let quads =
            client.call_on(&session, "DOM.getContentQuads", target.clone())["quads"].clone();
        assert_eq!(quads.as_array().unwrap().len(), 1, "{target}");
        assert_eq!(quads[0].as_array().unwrap().len(), 8, "{target}");

        client.call_on(&session, "DOM.scrollIntoViewIfNeeded", target.clone());
    }

    // Naming nothing at all is a driver bug, and reads as one.
    let error = client
        .try_call_on(&session, "DOM.getBoxModel", json!({}))
        .expect_err("no target must be refused");
    assert_eq!(error["code"], -32602, "{error}");
    assert_eq!(
        error["message"],
        "Either nodeId, backendNodeId or objectId must be specified"
    );
}

/// `resolveNode`'s `executionContextId` is validated and then ignored. The
/// validation is what stops a *stale* id from silently producing a handle into
/// the wrong document.
#[test]
fn resolve_node_accepts_an_announced_world_and_refuses_a_bogus_one() {
    let (harness, fixtures) = started();
    let (mut client, session, target) = loaded(&harness, &fixtures);
    client.call_on(&session, "Runtime.enable", json!({}));

    let node_id = heading_id(&mut client, &session);
    // A real `frameId`. It used to be literally `"ignored"`, which encoded the
    // old behaviour: the parameter was accepted and dropped. It is honoured now
    // (ADR-0035 D3) — a world of the wrong frame sees the wrong document — so
    // an id naming no frame is refused rather than quietly served.
    let main = client.call_on(
        &session,
        "Page.createIsolatedWorld",
        json!({ "frameId": target, "worldName": "util" }),
    )["executionContextId"]
        .as_i64()
        .expect("executionContextId");

    // The utility world's id — the one Puppeteer's `adoptBackendNode` always
    // sends — is accepted.
    let resolved = client.call_on(
        &session,
        "DOM.resolveNode",
        json!({ "backendNodeId": node_id, "executionContextId": main }),
    );
    assert_eq!(resolved["object"]["subtype"], "node");

    // Absent means the main world, and is fine.
    client.call_on(
        &session,
        "DOM.resolveNode",
        json!({ "backendNodeId": node_id }),
    );

    let error = client
        .try_call_on(
            &session,
            "DOM.resolveNode",
            json!({ "backendNodeId": node_id, "executionContextId": 987_654 }),
        )
        .expect_err("an id naming no world must be refused");
    assert_eq!(error["message"], "Cannot find context with specified id");
}

/// Navigation kills every id, and `DOM.documentUpdated` is how a driver hears
/// it — but only if it asked.
#[test]
fn navigation_updates_the_document_and_retires_every_id() {
    let (harness, fixtures) = started();
    let (mut client, session, _target) = loaded(&harness, &fixtures);
    let stale = heading_id(&mut client, &session);

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/next") }),
    );
    client.await_event("DOM.documentUpdated");

    let error = client
        .try_call_on(&session, "DOM.describeNode", json!({ "nodeId": stale }))
        .expect_err("an id from the previous document must not resolve");
    assert_eq!(error["message"], "No node with given id found");

    // A fresh `getDocument` works, and its ids are new.
    let fresh = document_id(&mut client, &session);
    assert_ne!(fresh, stale);
}

#[test]
fn document_updated_needs_dom_enable() {
    let (harness, fixtures) = started();
    // Deliberately *not* `loaded`: no `DOM.enable` here.
    let (mut client, session, _) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/") }),
    );
    client.await_event("Page.frameNavigated");
    let events = client.drain_events(std::time::Duration::from_millis(300));
    assert!(
        !events.iter().any(|e| e["method"] == "DOM.documentUpdated"),
        "a session that never sent DOM.enable must not receive DOM events: {events:?}"
    );
}

/// The refusal register (ADR-0031 D4): absent where the capability is absent,
/// a named reason where it exists or is scheduled.
///
/// `DOM.getFrameOwner` used to be the *absent* case — there were no nested
/// browsing contexts to own a frame. ADR-0035 gave the engine frames, so it is
/// implemented, and the refusal it can still produce is a named one: an id
/// that names no frame.
#[test]
fn the_refusal_register_distinguishes_absent_from_scheduled() {
    let harness = Harness::start();
    let (mut client, session, target) = harness.attached();

    let error = client
        .try_call_on(&session, "DOM.getFrameOwner", json!({ "frameId": "x" }))
        .unwrap_err();
    assert_eq!(
        error["code"], -32000,
        "a named refusal, not absent: {error}"
    );
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("frame"),
        "{error}"
    );

    // The top-level frame is a real frame with no owning element, which is a
    // different answer from "no such frame".
    let error = client
        .try_call_on(&session, "DOM.getFrameOwner", json!({ "frameId": target }))
        .unwrap_err();
    assert_eq!(error["code"], -32000, "{error}");
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("owner"),
        "{error}"
    );
}

/// `DOM.setFileInputFiles` landed with ADR-0032; the refusal this replaces was
/// the one ADR-0031 D4 recorded as *scheduled, not absent*.
#[test]
fn set_file_input_files_selects_and_fires_the_events() {
    let directory = std::env::temp_dir().join(format!("oxidepage-upload-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("hello.txt");
    std::fs::write(&path, b"hello upload").unwrap();

    let fixtures = Fixtures::start(vec![(
        "/upload.html",
        "<input id=f type=file>\
         <script>\
           window.seen = [];\
           document.getElementById('f').addEventListener('input', e => \
             window.seen.push(['input', e.isTrusted]));\
           document.getElementById('f').addEventListener('change', e => \
             window.seen.push(['change', e.isTrusted]));\
         </script>",
    )]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(&session, "DOM.enable", json!({}));
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/upload.html") }),
    );

    let document = client.call_on(&session, "DOM.getDocument", json!({ "depth": -1 }));
    let input = find_by_name(&document["root"], "INPUT").expect("the input");
    let node_id = input["nodeId"].as_i64().expect("nodeId");

    client.call_on(
        &session,
        "DOM.setFileInputFiles",
        json!({ "files": [path.to_string_lossy()], "nodeId": node_id }),
    );

    let files = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({
            "expression": "JSON.stringify([\
                 document.getElementById('f').files.length,\
                 document.getElementById('f').files[0].name,\
                 document.getElementById('f').files[0].size,\
                 window.seen])",
            "returnByValue": true,
        }),
    );
    assert_eq!(
        files["result"]["value"], r#"[1,"hello.txt",12,[["input",true],["change",true]]]"#,
        "the selection and its two *trusted* events"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn set_file_input_files_refuses_a_node_that_is_not_a_file_input() {
    let fixtures = Fixtures::start(vec![("/plain.html", "<p id=p>text</p>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(&session, "DOM.enable", json!({}));
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/plain.html") }),
    );
    let document = client.call_on(&session, "DOM.getDocument", json!({ "depth": -1 }));
    let paragraph = find_by_name(&document["root"], "P").expect("the paragraph");

    // ADR-0031's limit holds: with no `DataTransfer` there is nothing to set on
    // a non-input target, and saying so beats silently doing nothing.
    let error = client
        .try_call_on(
            &session,
            "DOM.setFileInputFiles",
            json!({ "files": [], "nodeId": paragraph["nodeId"] }),
        )
        .unwrap_err();
    assert_eq!(error["code"], -32000, "{error}");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("<input type=file>"),
        "{error}"
    );
}

#[test]
fn a_file_chooser_is_announced_only_when_intercepted() {
    let fixtures = Fixtures::start(vec![("/chooser.html", "<input id=f type=file multiple>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/chooser.html") }),
    );

    // Off by default: clicking records nothing, which is the honest headless
    // answer (ADR-0032 D12).
    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "document.getElementById('f').click()" }),
    );
    let quiet = client.drain_events(std::time::Duration::from_millis(300));
    assert!(
        !quiet
            .iter()
            .any(|e| e["method"] == "Page.fileChooserOpened"),
        "no chooser without interception: {quiet:?}"
    );

    client.call_on(
        &session,
        "Page.setInterceptFileChooserDialog",
        json!({ "enabled": true }),
    );
    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "document.getElementById('f').click()" }),
    );
    let chooser = client.await_event("Page.fileChooserOpened");
    assert_eq!(chooser["params"]["mode"], "selectMultiple");
}

/// The first node in `root`'s subtree whose `nodeName` matches.
fn find_by_name<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
    if node["nodeName"] == name {
        return Some(node);
    }
    node["children"]
        .as_array()?
        .iter()
        .find_map(|child| find_by_name(child, name))
}
