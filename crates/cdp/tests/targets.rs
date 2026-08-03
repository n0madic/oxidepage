//! The `Target` domain: discovery, attachment, and browser contexts.

mod common;

use std::time::Duration;

use common::Harness;
use serde_json::{Value, json};

/// How long to wait before concluding an event did *not* fire.
const SETTLE: Duration = Duration::from_millis(400);

#[test]
fn discovery_reports_targets_that_already_exist() {
    let harness = Harness::start();
    let mut client = harness.client();

    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let target_id = created["targetId"].as_str().unwrap().to_owned();

    // Turning discovery on after the fact must still report the page: this is
    // exactly what `puppeteer.connect()` to a running browser does, and without
    // the catch-up it finds no pages at all.
    client.call("Target.setDiscoverTargets", json!({ "discover": true }));
    let event = client.await_event("Target.targetCreated");
    assert_eq!(
        event["params"]["targetInfo"]["targetId"],
        target_id.as_str()
    );
    assert_eq!(event["params"]["targetInfo"]["type"], "page");
}

#[test]
fn discovery_reports_targets_created_later() {
    let harness = Harness::start();
    let mut client = harness.client();
    client.call("Target.setDiscoverTargets", json!({ "discover": true }));

    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let event = client.await_event("Target.targetCreated");
    assert_eq!(
        event["params"]["targetInfo"]["targetId"],
        created["targetId"]
    );
}

#[test]
fn no_target_events_arrive_before_discovery_is_enabled() {
    let harness = Harness::start();
    let mut client = harness.client();

    client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let events = client.drain_events(SETTLE);
    assert!(
        !events.iter().any(|e| e["method"] == "Target.targetCreated"),
        "a session that never enabled discovery must not receive it: {events:?}"
    );
}

#[test]
fn attaching_yields_a_session_and_announces_it() {
    let harness = Harness::start();
    let mut client = harness.client();

    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let target_id = created["targetId"].as_str().unwrap().to_owned();

    let attached = client.call(
        "Target.attachToTarget",
        json!({ "targetId": target_id, "flatten": true }),
    );
    let session_id = attached["sessionId"].as_str().unwrap().to_owned();
    assert_eq!(session_id.len(), 32, "session ids are 32 hex characters");

    let event = client.await_event("Target.attachedToTarget");
    assert_eq!(event["params"]["sessionId"], session_id.as_str());
    assert_eq!(
        event["params"]["targetInfo"]["targetId"],
        target_id.as_str()
    );
    // The target now reports itself attached, which is what `getTargets` and
    // `/json/list` both surface.
    assert_eq!(event["params"]["targetInfo"]["attached"], true);
}

#[test]
fn the_nested_session_mode_is_refused_rather_than_served_as_flat() {
    let harness = Harness::start();
    let mut client = harness.client();
    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));

    // Only flat mode exists. Quietly treating `flatten: false` as flat would
    // send a driver replies on an envelope shape it is not reading.
    let error = client
        .try_call(
            "Target.attachToTarget",
            json!({ "targetId": created["targetId"], "flatten": false }),
        )
        .expect_err("nested mode is not implemented");
    assert_eq!(error["code"], -32000);
    assert!(
        error["message"].as_str().unwrap().contains("flat"),
        "unhelpful message: {error}"
    );
}

#[test]
fn detaching_ends_the_session() {
    let harness = Harness::start();
    let mut client = harness.client();
    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let attached = client.call(
        "Target.attachToTarget",
        json!({ "targetId": created["targetId"], "flatten": true }),
    );
    let session_id = attached["sessionId"].as_str().unwrap().to_owned();

    client.call(
        "Target.detachFromTarget",
        json!({ "sessionId": session_id }),
    );
    let event = client.await_event("Target.detachedFromTarget");
    assert_eq!(event["params"]["sessionId"], session_id.as_str());

    // The session is gone, so a command on it is refused.
    let error = client
        .try_call_on(&session_id, "Target.getTargets", json!({}))
        .expect_err("session was detached");
    assert_eq!(error["code"], -32000);
}

#[test]
fn auto_attach_attaches_to_existing_and_future_targets() {
    let harness = Harness::start();
    let mut client = harness.client();

    let first = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    client.call(
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true }),
    );

    // The page that already existed arrives attached.
    let event = client.await_event("Target.attachedToTarget");
    assert_eq!(event["params"]["targetInfo"]["targetId"], first["targetId"]);

    // And so does the next one, without the driver asking.
    let second = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let event = client.await_event("Target.attachedToTarget");
    assert_eq!(
        event["params"]["targetInfo"]["targetId"],
        second["targetId"]
    );
}

#[test]
fn closing_a_target_destroys_it() {
    let harness = Harness::start();
    let mut client = harness.client();
    client.call("Target.setDiscoverTargets", json!({ "discover": true }));

    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let target_id = created["targetId"].as_str().unwrap().to_owned();
    let _ = client.await_event("Target.targetCreated");

    let closed = client.call("Target.closeTarget", json!({ "targetId": target_id }));
    assert_eq!(closed["success"], true);

    let event = client.await_event("Target.targetDestroyed");
    assert_eq!(event["params"]["targetId"], target_id.as_str());

    assert_eq!(
        client.call("Target.getTargets", json!({}))["targetInfos"],
        json!([])
    );
}

#[test]
fn closing_a_target_detaches_its_sessions() {
    let harness = Harness::start();
    let mut client = harness.client();
    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let target_id = created["targetId"].as_str().unwrap().to_owned();
    let attached = client.call(
        "Target.attachToTarget",
        json!({ "targetId": &target_id, "flatten": true }),
    );
    let session_id = attached["sessionId"].as_str().unwrap().to_owned();
    let _ = client.await_event("Target.attachedToTarget");

    client.call("Target.closeTarget", json!({ "targetId": target_id }));

    // A driver holding the session must be told it is dead, or it waits on a
    // page that will never answer again.
    let event = client.await_event("Target.detachedFromTarget");
    assert_eq!(event["params"]["sessionId"], session_id.as_str());
}

#[test]
fn an_unknown_target_id_is_refused_by_every_command_that_takes_one() {
    let harness = Harness::start();
    let mut client = harness.client();
    let ghost = json!({ "targetId": "ffffffffffffffffffffffffffffffff" });

    for method in [
        "Target.attachToTarget",
        "Target.closeTarget",
        "Target.activateTarget",
    ] {
        let error = match client.try_call(method, ghost.clone()) {
            Err(error) => error,
            Ok(result) => panic!("{method} accepted an unknown target: {result}"),
        };
        assert_eq!(error["code"], -32000, "{method} used the wrong error code");
    }
}

#[test]
fn activating_a_target_answers_without_a_window_manager() {
    let harness = Harness::start();
    let mut client = harness.client();
    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));

    // Bringing to front means nothing headless, but the command must answer:
    // Puppeteer calls it on every `page.bringToFront()` and on some navigations.
    client.call(
        "Target.activateTarget",
        json!({ "targetId": created["targetId"] }),
    );
}

#[test]
fn browser_contexts_are_created_listed_and_disposed() {
    let harness = Harness::start();
    let mut client = harness.client();

    // The default context is not listed: Puppeteer treats every listed id as a
    // disposable incognito context, and disposing the default one would take
    // the browser's own pages with it.
    assert_eq!(
        client.call("Target.getBrowserContexts", json!({}))["browserContextIds"],
        json!([])
    );

    let context = client.call("Target.createBrowserContext", json!({}));
    let context_id = context["browserContextId"].as_str().unwrap().to_owned();
    assert_eq!(context_id.len(), 32);

    let listed = client.call("Target.getBrowserContexts", json!({}));
    assert_eq!(listed["browserContextIds"], json!([context_id.as_str()]));

    // A page created in it reports its context, unlike one in the default.
    let scoped = client.call(
        "Target.createTarget",
        json!({ "url": "about:blank", "browserContextId": &context_id }),
    );
    let default = client.call("Target.createTarget", json!({ "url": "about:blank" }));

    let targets = client.call("Target.getTargets", json!({}));
    let infos = targets["targetInfos"].as_array().unwrap();
    let find = |id: &Value| {
        infos
            .iter()
            .find(|info| info["targetId"] == *id)
            .unwrap_or_else(|| panic!("target {id} missing"))
    };
    assert_eq!(
        find(&scoped["targetId"])["browserContextId"],
        context_id.as_str()
    );
    assert_eq!(find(&default["targetId"])["browserContextId"], Value::Null);

    client.call(
        "Target.disposeBrowserContext",
        json!({ "browserContextId": &context_id }),
    );
    assert_eq!(
        client.call("Target.getBrowserContexts", json!({}))["browserContextIds"],
        json!([])
    );
}

#[test]
fn disposing_an_unknown_context_is_refused() {
    let harness = Harness::start();
    let mut client = harness.client();
    let error = client
        .try_call(
            "Target.disposeBrowserContext",
            json!({ "browserContextId": "ffffffffffffffffffffffffffffffff" }),
        )
        .expect_err("unknown context");
    assert_eq!(error["code"], -32000);
}

#[test]
fn creating_a_target_in_an_unknown_context_is_refused() {
    let harness = Harness::start();
    let mut client = harness.client();
    let error = client
        .try_call(
            "Target.createTarget",
            json!({ "url": "about:blank", "browserContextId": "ffffffffffffffffffffffffffffffff" }),
        )
        .expect_err("unknown context");
    assert_eq!(error["code"], -32000);
}

#[test]
fn two_connections_see_the_same_targets_independently() {
    let harness = Harness::start();
    let mut first = harness.client();
    let mut second = harness.client();

    // Only the first enables discovery.
    first.call("Target.setDiscoverTargets", json!({ "discover": true }));
    let created = second.call("Target.createTarget", json!({ "url": "about:blank" }));

    let event = first.await_event("Target.targetCreated");
    assert_eq!(
        event["params"]["targetInfo"]["targetId"],
        created["targetId"]
    );

    // The second connection sees the target by polling, but got no event —
    // domain enablement is per connection, not per browser.
    assert_eq!(
        second.call("Target.getTargets", json!({}))["targetInfos"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let events = second.drain_events(SETTLE);
    assert!(
        !events.iter().any(|e| e["method"] == "Target.targetCreated"),
        "discovery leaked across connections: {events:?}"
    );
}
