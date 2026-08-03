//! The `Runtime` and `Log` domains over the wire.

mod common;

use std::time::Duration;

use common::{Fixtures, Harness};
use serde_json::json;

#[test]
fn evaluate_returns_a_primitive_by_value() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    let result = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "1 + 1", "returnByValue": true }),
    );
    assert_eq!(result["result"]["type"], "number");
    assert_eq!(result["result"]["value"], 2);
    assert!(result.get("exceptionDetails").is_none());
}

#[test]
fn a_by_value_object_arrives_as_structure_not_as_text() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    let result = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "({ a: [1, 2], b: 'x' })", "returnByValue": true }),
    );
    // A driver reads `result.value.a[1]`. Handing back the JSON *text* under
    // `value` would type-check on the wire and break every consumer.
    assert_eq!(result["result"]["value"]["a"][1], 2);
    assert_eq!(result["result"]["value"]["b"], "x");
    assert!(result["result"].get("objectId").is_none());
}

#[test]
fn an_object_comes_back_as_a_handle_that_later_commands_can_use() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    let result = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "({ n: 21 })" }),
    );
    let object_id = result["result"]["objectId"]
        .as_str()
        .expect("objectId")
        .to_owned();
    assert_eq!(result["result"]["type"], "object");

    let doubled = client.call_on(
        &session,
        "Runtime.callFunctionOn",
        json!({
            "functionDeclaration": "function () { return this.n * 2; }",
            "objectId": object_id,
            "returnByValue": true,
        }),
    );
    assert_eq!(doubled["result"]["value"], 42);
}

#[test]
fn call_function_on_passes_handles_and_literals_as_arguments() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    let holder = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "({ v: 'from-handle' })" }),
    );
    let result = client.call_on(
        &session,
        "Runtime.callFunctionOn",
        json!({
            "functionDeclaration": "(o, suffix) => o.v + suffix",
            "arguments": [
                { "objectId": holder["result"]["objectId"] },
                { "value": "!" },
            ],
            "returnByValue": true,
        }),
    );
    assert_eq!(result["result"]["value"], "from-handle!");
}

#[test]
fn get_properties_enumerates_a_handle() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    let object = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "({ x: 1, y: 'two' })" }),
    );

    let properties = client.call_on(
        &session,
        "Runtime.getProperties",
        json!({ "objectId": object["result"]["objectId"], "ownProperties": true }),
    );
    let names: Vec<&str> = properties["result"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(names, vec!["x", "y"]);
    assert_eq!(properties["result"][0]["value"]["value"], 1);
    assert_eq!(properties["result"][0]["isOwn"], true);
}

#[test]
fn releasing_a_handle_makes_later_use_an_error() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    let object = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "({})" }),
    );
    let object_id = object["result"]["objectId"].as_str().unwrap().to_owned();

    client.call_on(
        &session,
        "Runtime.releaseObject",
        json!({ "objectId": &object_id }),
    );

    let error = client
        .try_call_on(
            &session,
            "Runtime.getProperties",
            json!({ "objectId": &object_id }),
        )
        .expect_err("the handle was released");
    assert_eq!(error["code"], -32000);
}

#[test]
fn an_object_group_releases_together() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    let grouped = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "({})", "objectGroup": "probe" }),
    );
    let loose = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "({})" }),
    );

    client.call_on(
        &session,
        "Runtime.releaseObjectGroup",
        json!({ "objectGroup": "probe" }),
    );

    assert!(
        client
            .try_call_on(
                &session,
                "Runtime.getProperties",
                json!({ "objectId": grouped["result"]["objectId"] }),
            )
            .is_err()
    );
    // An ungrouped handle must survive: a driver keeps long-lived handles
    // outside any group precisely so a group sweep cannot take them.
    client.call_on(
        &session,
        "Runtime.getProperties",
        json!({ "objectId": loose["result"]["objectId"] }),
    );
}

#[test]
fn an_unknown_object_id_is_refused() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    for id in ["999999", "not-an-id"] {
        let error = match client.try_call_on(
            &session,
            "Runtime.getProperties",
            json!({ "objectId": id }),
        ) {
            Err(error) => error,
            Ok(result) => panic!("{id} was accepted: {result}"),
        };
        assert_eq!(error["code"], -32000, "{id}");
    }
}

#[test]
fn a_thrown_error_arrives_as_exception_details() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    let result = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "throw new TypeError('boom')" }),
    );
    let details = &result["exceptionDetails"];
    assert!(
        details["text"].as_str().unwrap().contains("TypeError"),
        "the classification must survive: {details}"
    );
    assert!(details["text"].as_str().unwrap().contains("boom"));
    assert_eq!(details["exception"]["subtype"], "error");
}

#[test]
fn await_promise_settles_before_answering() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    // Resolved by a timer, so a state read would say `pending` forever — the
    // command has to actually run the event loop.
    let result = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({
            "expression": "new Promise(r => setTimeout(() => r('late'), 10))",
            "awaitPromise": true,
            "returnByValue": true,
        }),
    );
    assert!(result.get("exceptionDetails").is_none(), "{result}");
    assert_eq!(result["result"]["value"], "late");
}

#[test]
fn a_rejected_promise_becomes_exception_details() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    let result = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({
            "expression": "Promise.reject(new Error('nope'))",
            "awaitPromise": true,
        }),
    );
    assert!(
        result["exceptionDetails"]["text"]
            .as_str()
            .unwrap()
            .contains("nope"),
        "{result}"
    );
}

#[test]
fn runtime_enable_reports_the_execution_context() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    client.call_on(&session, "Runtime.enable", json!({}));
    let event = client.await_event("Runtime.executionContextCreated");
    let context = &event["params"]["context"];
    assert!(context["id"].as_u64().is_some());
    // Both drivers key off `auxData`: `frameId` maps the context to a frame and
    // `isDefault` distinguishes the main world from an isolated one.
    assert!(!context["auxData"]["frameId"].as_str().unwrap().is_empty());
    assert_eq!(context["auxData"]["isDefault"], true);
}

#[test]
fn console_calls_reach_the_driver() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(&session, "Runtime.enable", json!({}));
    let _ = client.await_event("Runtime.executionContextCreated");

    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "console.warn('careful', 42)" }),
    );

    let event = client.await_event("Runtime.consoleAPICalled");
    assert_eq!(event["params"]["type"], "warning");
    let args = event["params"]["args"].as_array().unwrap();
    assert_eq!(args.len(), 2);
    assert_eq!(args[0]["value"], "careful");
    assert_eq!(args[1]["value"], "42");
}

#[test]
fn no_console_events_reach_a_session_that_did_not_enable_runtime() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    // `Page` is enabled by the harness; `Runtime` deliberately is not.
    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "console.log('quiet')" }),
    );
    let events = client.drain_events(Duration::from_millis(300));
    assert!(
        !events
            .iter()
            .any(|e| e["method"] == "Runtime.consoleAPICalled"),
        "console leaked to a session that never enabled Runtime: {events:?}"
    );
}

#[test]
fn an_uncaught_error_becomes_exception_thrown() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(&session, "Runtime.enable", json!({}));
    let _ = client.await_event("Runtime.executionContextCreated");

    // Thrown from a task, so it is uncaught rather than the result of the
    // evaluation itself.
    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "setTimeout(() => { throw new RangeError('late boom'); }, 0)" }),
    );

    let event = client.await_event("Runtime.exceptionThrown");
    let text = event["params"]["exceptionDetails"]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("late boom"), "{text}");
    assert!(text.contains("RangeError"), "{text}");
}

#[test]
fn a_resource_failure_goes_to_log_not_to_runtime() {
    let fixtures = Fixtures::start(vec![(
        "/a",
        // A stylesheet that is not there: a resource failure, not an uncaught
        // exception. Filing it under `exceptionThrown` would make that event
        // mean two different things.
        "<!doctype html><link rel=stylesheet href=/missing.css><title>A</title>",
    )]);
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(&session, "Runtime.enable", json!({}));
    client.call_on(&session, "Log.enable", json!({}));
    let _ = client.await_event("Runtime.executionContextCreated");

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    let events = client.drain_events(Duration::from_millis(800));
    let log_entries: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["method"] == "Log.entryAdded")
        .collect();
    assert!(
        !log_entries.is_empty(),
        "a failed subresource must be reported: {events:?}"
    );
    assert_eq!(log_entries[0]["params"]["entry"]["source"], "network");
    assert!(
        !events
            .iter()
            .any(|e| e["method"] == "Runtime.exceptionThrown"),
        "a resource failure is not an uncaught exception: {events:?}"
    );
}

#[test]
fn a_binding_reports_its_payload() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    client.call_on(
        &session,
        "Runtime.addBinding",
        json!({ "name": "__report" }),
    );
    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "__report('from the page')" }),
    );

    let event = client.await_event("Runtime.bindingCalled");
    assert_eq!(event["params"]["name"], "__report");
    assert_eq!(event["params"]["payload"], "from the page");
}

#[test]
fn a_binding_asked_for_an_isolated_world_lands_in_the_main_one() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    // There is one world. `executionContextName` is accepted and mapped to it:
    // a documented divergence (ADR-0030), not a stub — the binding really is
    // installed, it is simply not hidden from page script. Refusing instead
    // makes `browser.newPage()` throw, because both drivers ask for a utility
    // world while setting a page up.
    client.call_on(
        &session,
        "Runtime.addBinding",
        json!({ "name": "__world", "executionContextName": "utility" }),
    );
    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "__world('reachable')" }),
    );
    let event = client.await_event("Runtime.bindingCalled");
    assert_eq!(event["params"]["payload"], "reachable");
}

#[test]
fn navigation_invalidates_handles_and_announces_a_new_context() {
    let fixtures = Fixtures::start(vec![("/a", "<!doctype html><title>A</title>")]);
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(&session, "Runtime.enable", json!({}));
    let first = client.await_event("Runtime.executionContextCreated");

    let object = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "({})" }),
    );
    let object_id = object["result"]["objectId"].as_str().unwrap().to_owned();

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    // Every handle named a value of the outgoing document.
    let error = client
        .try_call_on(
            &session,
            "Runtime.getProperties",
            json!({ "objectId": &object_id }),
        )
        .expect_err("handles do not survive a navigation");
    assert_eq!(error["code"], -32000);

    // And the context id moved, which is how a driver learns that without
    // probing each handle it holds.
    client.call_on(&session, "Runtime.disable", json!({}));
    client.call_on(&session, "Runtime.enable", json!({}));
    let second = client.await_event("Runtime.executionContextCreated");
    assert_ne!(
        first["params"]["context"]["id"],
        second["params"]["context"]["id"]
    );
}
