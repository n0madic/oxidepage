//! The `Network`, `Emulation` and `Security` domains.

mod common;

use std::time::Duration;

use common::{Fixtures, Harness};
use serde_json::{Value, json};

#[test]
fn a_navigation_reports_the_request_and_the_response() {
    let fixtures = Fixtures::start(vec![("/a", "<!doctype html><title>A</title>")]);
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(&session, "Network.enable", json!({}));

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    let events = client.drain_events(Duration::from_millis(600));
    let names: Vec<&str> = events.iter().filter_map(|e| e["method"].as_str()).collect();
    for expected in [
        "Network.requestWillBeSent",
        "Network.responseReceived",
        "Network.loadingFinished",
    ] {
        assert!(
            names.contains(&expected),
            "missing {expected}; saw {names:?}"
        );
    }

    let sent = events
        .iter()
        .find(|e| e["method"] == "Network.requestWillBeSent")
        .unwrap();
    assert_eq!(sent["params"]["request"]["url"], fixtures.url("/a"));
    assert_eq!(sent["params"]["request"]["method"], "GET");

    let received = events
        .iter()
        .find(|e| e["method"] == "Network.responseReceived")
        .unwrap();
    assert_eq!(received["params"]["response"]["status"], 200);
    assert_eq!(received["params"]["response"]["mimeType"], "text/html");
    // The main document goes through the *synchronous* fetch path, which
    // produces no `NetEvent` at all — a hook on the async path alone would miss
    // exactly the request a driver cares most about.
    assert_eq!(received["params"]["requestId"], sent["params"]["requestId"]);
}

/// A request a frame started reports **that frame's** id.
///
/// `request.frame()` is how a driver attributes a request, and reporting the
/// target id for everything told it the page had asked for all of them
/// (ADR-0035 D9). The page's own document load still reports the target id —
/// there is no frame to name before it commits.
#[test]
fn a_network_event_names_the_frame_that_started_it() {
    let fixtures = Fixtures::start(vec![
        (
            "/frames",
            "<!doctype html><title>Frames</title>\
             <iframe id='f' src='/inner'></iframe>",
        ),
        (
            "/inner",
            "<!doctype html><title>Inner</title><link rel=stylesheet href='/sheet.css'>",
        ),
        ("/sheet.css", "p { color: red }"),
    ]);
    let harness = Harness::start();
    let (mut client, session, target) = harness.attached();
    client.call_on(&session, "Network.enable", json!({}));
    client.call_on(&session, "Page.enable", json!({}));
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/frames") }),
    );

    let events = client.drain_events(Duration::from_millis(800));
    let sent: Vec<&Value> = events
        .iter()
        .filter(|e| e["method"] == "Network.requestWillBeSent")
        .collect();
    let frame_of = |suffix: &str| -> String {
        sent.iter()
            .find(|e| {
                e["params"]["request"]["url"]
                    .as_str()
                    .is_some_and(|url| url.ends_with(suffix))
            })
            .unwrap_or_else(|| panic!("no request for {suffix}: {sent:?}"))["params"]["frameId"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };

    assert_eq!(
        frame_of("/frames"),
        target,
        "the page's own document has no frame to name yet"
    );
    let inner = frame_of("/inner");
    assert_ne!(inner, target, "the frame's document is the frame's request");
    assert_eq!(
        frame_of("/sheet.css"),
        inner,
        "and so is a stylesheet that document asked for"
    );
}

#[test]
fn a_response_body_reads_back() {
    let body = "<!doctype html><title>Readable</title><p>hello</p>";
    let fixtures = Fixtures::start(vec![("/a", body)]);
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(&session, "Network.enable", json!({}));
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    let events = client.drain_events(Duration::from_millis(600));
    let request_id = events
        .iter()
        .find(|e| e["method"] == "Network.responseReceived")
        .map(|e| e["params"]["requestId"].clone())
        .expect("responseReceived");

    let response = client.call_on(
        &session,
        "Network.getResponseBody",
        json!({ "requestId": request_id }),
    );
    assert_eq!(response["base64Encoded"], false);
    assert_eq!(response["body"], body);
}

#[test]
fn an_unknown_request_id_is_refused() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(&session, "Network.enable", json!({}));

    for id in ["", "nope", "othertarget.1v1"] {
        let error = match client.try_call_on(
            &session,
            "Network.getResponseBody",
            json!({ "requestId": id }),
        ) {
            Err(error) => error,
            Ok(result) => panic!("{id:?} was accepted: {result}"),
        };
        assert_eq!(error["code"], -32000, "{id:?}");
    }
}

#[test]
fn no_network_events_reach_a_session_that_did_not_enable_the_domain() {
    let fixtures = Fixtures::start(vec![("/a", "<!doctype html><title>A</title>")]);
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    // `Page` is on (the harness enables it); `Network` deliberately is not.
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );
    let events = client.drain_events(Duration::from_millis(400));
    assert!(
        !events.iter().any(|e| e["method"]
            .as_str()
            .is_some_and(|m| m.starts_with("Network."))),
        "network events leaked: {events:?}"
    );
}

#[test]
fn cookies_are_set_read_and_deleted() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    assert_eq!(
        client.call_on(&session, "Network.getAllCookies", json!({}))["cookies"],
        json!([])
    );

    let set = client.call_on(
        &session,
        "Network.setCookie",
        json!({
            "name": "sid",
            "value": "abc",
            "url": "https://example.com/app",
            "httpOnly": true,
        }),
    );
    assert_eq!(set["success"], true);

    let all = client.call_on(&session, "Network.getAllCookies", json!({}));
    let cookies = all["cookies"].as_array().unwrap();
    assert_eq!(cookies.len(), 1);
    assert_eq!(cookies[0]["name"], "sid");
    assert_eq!(cookies[0]["value"], "abc");
    assert_eq!(cookies[0]["domain"], "example.com");
    // An inspection sees `HttpOnly`, unlike `document.cookie` — that is the
    // point of the command.
    assert_eq!(cookies[0]["httpOnly"], true);
    // No expiry: a session cookie, which CDP spells as -1.
    assert_eq!(cookies[0]["session"], true);
    assert_eq!(cookies[0]["expires"], -1.0);

    client.call_on(
        &session,
        "Network.deleteCookies",
        json!({ "name": "sid", "url": "https://example.com/app" }),
    );
    assert_eq!(
        client.call_on(&session, "Network.getAllCookies", json!({}))["cookies"],
        json!([])
    );
}

#[test]
fn get_cookies_filters_by_url() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    client.call_on(
        &session,
        "Network.setCookies",
        json!({
            "cookies": [
                { "name": "a", "value": "1", "url": "https://one.example/" },
                { "name": "b", "value": "2", "url": "https://two.example/" },
            ]
        }),
    );

    let scoped = client.call_on(
        &session,
        "Network.getCookies",
        json!({ "urls": ["https://one.example/"] }),
    );
    let names: Vec<&str> = scoped["cookies"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert_eq!(names, vec!["a"]);

    // Two URLs of the same site match the same cookie; it must be reported
    // once, or a driver's `cookies.length` is wrong.
    let duplicated = client.call_on(
        &session,
        "Network.getCookies",
        json!({ "urls": ["https://one.example/", "https://one.example/deeper"] }),
    );
    assert_eq!(duplicated["cookies"].as_array().unwrap().len(), 1);
}

#[test]
fn clear_browser_cookies_empties_the_jar() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(
        &session,
        "Network.setCookie",
        json!({ "name": "a", "value": "1", "url": "https://example.com/" }),
    );
    client.call_on(&session, "Network.clearBrowserCookies", json!({}));
    assert_eq!(
        client.call_on(&session, "Network.getAllCookies", json!({}))["cookies"],
        json!([])
    );
}

#[test]
fn a_cookie_needs_a_url_or_a_domain() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    let error = client
        .try_call_on(
            &session,
            "Network.setCookie",
            json!({ "name": "a", "value": "1" }),
        )
        .expect_err("neither url nor domain");
    assert_eq!(error["code"], -32602);
}

#[test]
fn cookies_are_isolated_between_browser_contexts() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(
        &session,
        "Network.setCookie",
        json!({ "name": "a", "value": "1", "url": "https://example.com/" }),
    );

    // A page in a fresh context has its own jar (ADR-0027 D7).
    let context = client.call("Target.createBrowserContext", json!({}));
    let other = client.call(
        "Target.createTarget",
        json!({ "url": "about:blank", "browserContextId": context["browserContextId"] }),
    );
    let attached = client.call(
        "Target.attachToTarget",
        json!({ "targetId": other["targetId"], "flatten": true }),
    );
    let other_session = attached["sessionId"].as_str().unwrap().to_owned();

    assert_eq!(
        client.call_on(&other_session, "Network.getAllCookies", json!({}))["cookies"],
        json!([]),
        "a second context must not see the first's cookies"
    );
}

#[test]
fn overrides_the_engine_cannot_perform_are_refused() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    // Each of these would be a silent lie as `{}`: a test that sets a timezone
    // and then asserts on a formatted date must fail at the setter.
    for (method, params) in [
        (
            "Emulation.setTimezoneOverride",
            json!({ "timezoneId": "Europe/Kyiv" }),
        ),
        (
            "Emulation.setGeolocationOverride",
            json!({ "latitude": 50.4, "longitude": 30.5, "accuracy": 1 }),
        ),
        (
            "Emulation.setEmitTouchEventsForMouse",
            json!({ "enabled": true }),
        ),
        (
            "Emulation.setUserAgentOverride",
            json!({ "userAgent": "Custom/1.0" }),
        ),
        ("Emulation.setEmulatedMedia", json!({ "media": "print" })),
        (
            "Emulation.setScriptExecutionDisabled",
            json!({ "value": true }),
        ),
        (
            "Security.setIgnoreCertificateErrors",
            json!({ "ignore": true }),
        ),
    ] {
        let error = match client.try_call_on(&session, method, params) {
            Err(error) => error,
            Ok(result) => panic!("{method} silently accepted: {result}"),
        };
        assert_eq!(error["code"], -32000, "{method}");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("not implemented"),
            "{method}: unhelpful message {error}"
        );
    }
}

#[test]
fn the_no_op_forms_of_those_commands_are_accepted() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    // Asking for the state that already holds is not a lie, so it answers.
    client.call_on(
        &session,
        "Network.setCacheDisabled",
        json!({ "cacheDisabled": false }),
    );
    client.call_on(
        &session,
        "Emulation.setEmulatedMedia",
        json!({ "media": "screen" }),
    );
    client.call_on(
        &session,
        "Emulation.setScriptExecutionDisabled",
        json!({ "value": false }),
    );
    client.call_on(
        &session,
        "Security.setIgnoreCertificateErrors",
        json!({ "ignore": false }),
    );
    // Playwright sends this unconditionally; a headless page is always focused.
    client.call_on(
        &session,
        "Emulation.setFocusEmulationEnabled",
        json!({ "enabled": true }),
    );
    client.call_on(
        &session,
        "Network.setExtraHTTPHeaders",
        json!({ "headers": {} }),
    );
}

#[test]
fn device_metrics_change_the_viewport_the_page_sees() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    client.call_on(
        &session,
        "Emulation.setDeviceMetricsOverride",
        json!({ "width": 500, "height": 400, "deviceScaleFactor": 2, "mobile": false }),
    );

    let size = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({
            "expression": "[innerWidth, innerHeight, devicePixelRatio]",
            "returnByValue": true,
        }),
    );
    let dimensions = size["result"]["value"].as_array().expect("array");
    let numbers: Vec<f64> = dimensions
        .iter()
        .filter_map(serde_json::Value::as_f64)
        .collect();
    assert_eq!(numbers, vec![500.0, 400.0, 2.0]);

    // Zero means "do not override", not "a zero-area viewport".
    client.call_on(
        &session,
        "Emulation.setDeviceMetricsOverride",
        json!({ "width": 0, "height": 0, "deviceScaleFactor": 0, "mobile": false }),
    );
    let unchanged = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "innerWidth", "returnByValue": true }),
    );
    assert_eq!(unchanged["result"]["value"], 500);
}

#[test]
fn a_failed_request_is_reported_as_such() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(&session, "Network.enable", json!({}));

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": "http://127.0.0.1:1/nothing-listens-here" }),
    );

    let events = client.drain_events(Duration::from_millis(600));
    let failed: Vec<&Value> = events
        .iter()
        .filter(|e| e["method"] == "Network.loadingFailed")
        .collect();
    assert!(!failed.is_empty(), "expected loadingFailed: {events:?}");
    assert!(
        !failed[0]["params"]["errorText"]
            .as_str()
            .unwrap()
            .is_empty()
    );
}
