//! The `Fetch` domain over a real socket (ADR-0032).
//!
//! End to end rather than direct calls into `dispatch`, for the reason
//! `common/mod.rs` gives: the parts most likely to break — which lane a command
//! lands on, whether `requestPaused` actually reaches the wire, whether its id
//! matches the one `requestWillBeSent` carried — only exist once there is a
//! socket.

mod common;

use common::{Fixtures, Harness};
use serde_json::json;

/// Turns on `Network` + `Fetch` for a session, the pair a driver enables
/// together.
fn intercept(client: &mut common::Client, session: &str, patterns: serde_json::Value) {
    client.call_on(session, "Network.enable", json!({}));
    client.call_on(
        session,
        "Fetch.enable",
        json!({ "patterns": patterns, "handleAuthRequests": true }),
    );
}

#[test]
fn a_paused_request_carries_the_id_network_announced() {
    // The pairing Puppeteer's `NetworkManager` does. If `networkId` and the
    // `requestWillBeSent.requestId` disagree it drops the request entirely, and
    // `page.setRequestInterception` silently stops working.
    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    intercept(&mut client, &session, json!([{ "urlPattern": "*" }]));

    let navigate = client.dispatch(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );

    let paused = client.await_event("Fetch.requestPaused");
    let request_id = paused["params"]["requestId"].as_str().unwrap().to_owned();
    assert_eq!(paused["params"]["networkId"], json!(request_id));
    assert_eq!(paused["params"]["resourceType"], "Document");
    assert_eq!(
        paused["params"]["request"]["url"],
        json!(fixtures.url("/index.html")),
        "the request must be reported as announced"
    );

    // The pause is resolved on the *priority* lane while `Page.navigate` still
    // occupies the session lane. Without that (ADR-0032 D4) this deadlocks.
    client.call_on(
        &session,
        "Fetch.continueRequest",
        json!({ "requestId": request_id }),
    );
    client.collect(navigate).expect("navigate completed");

    let announced = client.await_event("Network.requestWillBeSent");
    assert_eq!(
        announced["params"]["requestId"],
        json!(request_id),
        "the paused id and the announced id are one id"
    );
    assert_eq!(announced["params"]["type"], "Document");
    assert_eq!(
        announced["params"]["loaderId"],
        json!(request_id),
        "a navigation request's id is its loaderId (D6a)"
    );
}

#[test]
fn a_fulfilled_request_replaces_the_document() {
    let fixtures = Fixtures::start(vec![("/real.html", "<title>real</title>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    intercept(&mut client, &session, json!([{ "urlPattern": "*" }]));

    let navigate = client.dispatch(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/real.html") }),
    );
    let paused = client.await_event("Fetch.requestPaused");
    let request_id = paused["params"]["requestId"].as_str().unwrap().to_owned();

    let body = oxidepage_cdp::base64::encode(b"<title>stubbed</title>");
    client.call_on(
        &session,
        "Fetch.fulfillRequest",
        json!({
            "requestId": request_id,
            "responseCode": 200,
            "responseHeaders": [{ "name": "content-type", "value": "text/html" }],
            "body": body,
        }),
    );
    client.collect(navigate).expect("navigate completed");

    let title = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "document.title", "returnByValue": true }),
    );
    assert_eq!(title["result"]["value"], "stubbed");
}

#[test]
fn a_failed_request_reports_chromes_error_text() {
    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    intercept(&mut client, &session, json!([{ "urlPattern": "*" }]));

    let navigate = client.dispatch(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );
    let paused = client.await_event("Fetch.requestPaused");
    let request_id = paused["params"]["requestId"].as_str().unwrap().to_owned();

    client.call_on(
        &session,
        "Fetch.failRequest",
        json!({ "requestId": request_id, "errorReason": "BlockedByClient" }),
    );
    // A failed navigation is not a protocol error: Chrome answers with
    // `errorText`, and Puppeteer turns that into a rejected `page.goto`.
    let outcome = client.collect(navigate).expect("navigate answered");
    // Equality throughout: `errorReason` round-trips to a driver by *name*, and
    // a substring assertion would pass while a prefixed `blocked: net::ERR_…`
    // broke every driver comparing it.
    assert_eq!(
        outcome["errorText"], "net::ERR_BLOCKED_BY_CLIENT",
        "a blocked navigation must report the abort reason verbatim: {outcome}"
    );

    let failed = client.await_event("Network.loadingFailed");
    assert_eq!(
        failed["params"]["errorText"], "net::ERR_BLOCKED_BY_CLIENT",
        "got {}",
        failed["params"]["errorText"]
    );
}

#[test]
fn a_url_override_is_validated_on_the_command() {
    // Refused *here*, at the pause boundary, rather than left to fail as a
    // confusing network error minutes later (ADR-0032 D5).
    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    intercept(&mut client, &session, json!([{ "urlPattern": "*" }]));

    let navigate = client.dispatch(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );
    let paused = client.await_event("Fetch.requestPaused");
    let request_id = paused["params"]["requestId"].as_str().unwrap().to_owned();

    for bad in ["not a url", "file:///etc/passwd", "javascript:1"] {
        let refused = client.try_call_on(
            &session,
            "Fetch.continueRequest",
            json!({ "requestId": request_id, "url": bad }),
        );
        assert!(refused.is_err(), "`{bad}` was accepted as a URL override");
    }

    // Refusing must not have spent the pause: the request is still resolvable.
    client.call_on(
        &session,
        "Fetch.continueRequest",
        json!({ "requestId": request_id }),
    );
    client.collect(navigate).expect("navigate completed");
}

#[test]
fn a_second_resolution_is_refused_by_name() {
    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    intercept(&mut client, &session, json!([{ "urlPattern": "*" }]));

    let navigate = client.dispatch(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );
    let paused = client.await_event("Fetch.requestPaused");
    let request_id = paused["params"]["requestId"].as_str().unwrap().to_owned();

    client.call_on(
        &session,
        "Fetch.continueRequest",
        json!({ "requestId": &request_id }),
    );
    let second = client.try_call_on(
        &session,
        "Fetch.continueRequest",
        json!({ "requestId": &request_id }),
    );
    let error = second.expect_err("the second resolution must be refused");
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Invalid InterceptionId"),
        "got {error}"
    );
    client.collect(navigate).expect("navigate completed");
}

#[test]
fn disable_releases_everything_it_was_holding() {
    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    intercept(&mut client, &session, json!([{ "urlPattern": "*" }]));

    let navigate = client.dispatch(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );
    let _paused = client.await_event("Fetch.requestPaused");

    // `Fetch.disable` is on the priority lane for exactly this: the navigation
    // it must release still owns the session lane.
    client.call_on(&session, "Fetch.disable", json!({}));
    client.collect(navigate).expect("disable released the load");

    let title = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "document.title", "returnByValue": true }),
    );
    assert_eq!(title["result"]["value"], "doc");
}

#[test]
fn a_closed_socket_releases_everything_it_was_holding() {
    // One of the four explicit release paths (ADR-0032 D7), and the one with no
    // automatic signal behind it: the page owns a sender on the decision
    // channel — deliberately, so a receiver whose only sender lived on the
    // driver side could never become permanently ready in the event loop's
    // `Select` — which means the channel never disconnects when a driver dies.
    //
    // Two connections, because the interceptor has to be able to *vanish* while
    // the navigation is still in flight. The interceptor drives `Fetch`; the
    // other drives the navigation.
    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut driver, driver_session, target) = harness.attached();

    let (mut interceptor, interceptor_session) = harness.attach_existing(&target);
    intercept(
        &mut interceptor,
        &interceptor_session,
        json!([{ "urlPattern": "*" }]),
    );

    let navigate = driver.dispatch(
        &driver_session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );
    let _paused = interceptor.await_event("Fetch.requestPaused");

    // The interceptor goes away without answering. Released as `Continue`, not
    // `Fail`: a driver that merely crashed must not break the page.
    drop(interceptor);

    let outcome = driver
        .collect(navigate)
        .expect("a closed interceptor released the paused load");
    assert!(
        outcome.get("errorText").is_none(),
        "the release is `Continue`, not `Fail`: {outcome}"
    );
    let title = driver.call_on(
        &driver_session,
        "Runtime.evaluate",
        json!({ "expression": "document.title", "returnByValue": true }),
    );
    assert_eq!(title["result"]["value"], "doc");
}

/// `Browser.close` must not sit out the intercept timeout on a paused page.
///
/// `Target.closeTarget` always released first; `Browser.close` went straight to
/// `Browser::close`. A page parked on the *blocking* pause of its own document
/// is inside `await_decision`, not at a wait point, so it services no job at all
/// — the close is answered only when the pause times out, by which point
/// `join_bounded` has given up and **detached** the thread, leaking it and its
/// `Page`. Puppeteer closes the browser from a `finally` block, so a driver that
/// simply errored out mid-interception hits this every time.
#[test]
fn closing_the_browser_releases_a_paused_page_instead_of_waiting_it_out() {
    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut driver, driver_session, target) = harness.attached();

    let (mut interceptor, interceptor_session) = harness.attach_existing(&target);
    intercept(
        &mut interceptor,
        &interceptor_session,
        json!([{ "urlPattern": "*" }]),
    );

    let _navigate = driver.dispatch(
        &driver_session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );
    let _paused = interceptor.await_event("Fetch.requestPaused");

    // Closed with the pause still outstanding — nobody ever answers it.
    let started = std::time::Instant::now();
    driver.call("Browser.close", json!({}));
    let elapsed = started.elapsed();

    // Comfortably under both the per-page close timeout and the intercept
    // timeout, either of which the unreleased pause would have burned in full.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "closing a paused browser took {elapsed:?}; the pause was not released"
    );
}

#[test]
fn detaching_releases_everything_the_session_was_holding() {
    // The same release path reached by `Target.detachFromTarget`. A session
    // belongs to the connection that opened it, so this detaches its own —
    // which means the navigation has to be driven from a second connection.
    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut driver, driver_session, target) = harness.attached();

    let (mut interceptor, interceptor_session) = harness.attach_existing(&target);
    intercept(
        &mut interceptor,
        &interceptor_session,
        json!([{ "urlPattern": "*" }]),
    );

    let navigate = driver.dispatch(
        &driver_session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );
    let _paused = interceptor.await_event("Fetch.requestPaused");

    interceptor.call_on(
        &interceptor_session,
        "Target.detachFromTarget",
        json!({ "sessionId": &interceptor_session }),
    );

    driver
        .collect(navigate)
        .expect("detaching released the paused load");
}

#[test]
fn a_response_stage_pattern_is_refused_rather_than_downgraded() {
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();

    let refused = client.try_call_on(
        &session,
        "Fetch.enable",
        json!({ "patterns": [{ "urlPattern": "*", "requestStage": "Response" }] }),
    );
    // Serving these at the Request stage instead would have a driver rewriting
    // the wrong half of every exchange.
    assert!(refused.is_err(), "requestStage: Response must be refused");
}

#[test]
fn the_response_side_commands_are_absent() {
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    for method in [
        "Fetch.continueResponse",
        "Fetch.getResponseBody",
        "Fetch.takeResponseBodyAsStream",
    ] {
        assert!(
            client.try_call_on(&session, method, json!({})).is_err(),
            "{method} answered"
        );
    }
}

#[test]
fn enable_with_no_params_is_accepted() {
    // Puppeteer sends `Fetch.enable` with `handleAuthRequests` and nothing else,
    // and some drivers send it bare. A deserialization error here would make
    // `page.setRequestInterception(true)` throw.
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(&session, "Fetch.enable", json!({}));
    client.call_on(&session, "Fetch.disable", json!({}));
}

#[test]
fn an_unknown_resource_type_in_a_pattern_is_refused() {
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    let refused = client.try_call_on(
        &session,
        "Fetch.enable",
        json!({ "patterns": [{ "urlPattern": "*", "resourceType": "Hologram" }] }),
    );
    assert!(refused.is_err());
}

// === Network.emulateNetworkConditions (ADR-0032 D9) ===

#[test]
fn offline_emulation_fails_a_navigation() {
    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(&session, "Network.enable", json!({}));
    client.call_on(
        &session,
        "Network.emulateNetworkConditions",
        json!({ "offline": true, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1 }),
    );

    let outcome = client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );
    assert_eq!(
        outcome["errorText"], "net::ERR_INTERNET_DISCONNECTED",
        "an offline navigation must report the disconnection verbatim: {outcome}"
    );

    // And turning it back off restores the page.
    client.call_on(
        &session,
        "Network.emulateNetworkConditions",
        json!({ "offline": false, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1 }),
    );
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );
}

#[test]
fn bandwidth_shaping_is_refused_by_name() {
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    let refused = client.try_call_on(
        &session,
        "Network.emulateNetworkConditions",
        json!({
            "offline": false,
            "latency": 0,
            "downloadThroughput": 100_000,
            "uploadThroughput": -1,
        }),
    );
    let error = refused.expect_err("throughput must be refused, not approximated");
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("downloadThroughput"),
        "the refusal must name the member: {error}"
    );
}

/// One session's `Fetch.disable` must not turn interception off for another
/// session attached to the same target.
///
/// The intercept config is **page-wide** while `flags.fetch` is per session, so
/// a `Fetch.disable` used to clear the config for everyone while leaving the
/// other session's flag `true` — it stopped receiving `Fetch.requestPaused`
/// with no way to observe why. `Target.attachToTarget` allows two sessions on
/// one target and Puppeteer's `createCDPSession` produces exactly that.
#[test]
fn one_sessions_fetch_disable_leaves_another_sessions_interception_on() {
    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut client, first, target) = harness.attached();
    let (mut other, second) = harness.attach_existing(&target);

    client.call_on(&first, "Fetch.enable", json!({}));
    other.call_on(&second, "Fetch.enable", json!({}));

    // The first session bows out; the second still wants interception.
    client.call_on(&first, "Fetch.disable", json!({}));

    // A navigation must still pause for the session that never disabled.
    let navigate = other.dispatch(
        &second,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );
    let paused = other.await_event("Fetch.requestPaused");
    let request_id = paused["params"]["requestId"].as_str().unwrap().to_owned();
    other.call_on(
        &second,
        "Fetch.continueRequest",
        json!({ "requestId": request_id }),
    );
    other.collect(navigate).expect("the navigation completes");
}
