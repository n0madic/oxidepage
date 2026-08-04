//! Regressions for defects found in review of the transport (ADR-0030).
//!
//! Each test here corresponds to a bug that shipped in the first cut of the
//! endpoint and would otherwise be invisible: none of them fail on a
//! well-behaved local client, which is exactly why they need pinning.

mod common;

use std::time::Duration;

use common::{Fixtures, Harness};
use oxidepage_cdp::{CdpServer, ServerOptions};
use oxidepage_engine::{Browser, BrowserOptions};
use serde_json::json;

#[test]
fn a_request_split_across_tcp_segments_is_still_served() {
    let harness = Harness::start();

    // The router used to `peek` the request line without consuming it. `peek`
    // returns whatever the kernel holds *now*, so a request line delivered in
    // two segments never completed: the loop grew its window until it hit the
    // 64 KiB cap and then dropped the connection without answering. A peer is
    // entitled to split anywhere, so this must work.
    let response = harness.http_raw_chunked(&[
        "GET /json/vers",
        "ion HTTP/1.1\r\nHost: 127.0.0.1\r\n",
        "Connection: close\r\n\r\n",
    ]);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "a split request line must still be served, got:\n{response}"
    );
    assert!(
        response.contains("webSocketDebuggerUrl"),
        "body looks wrong:\n{response}"
    );
}

#[test]
fn a_head_split_mid_headers_is_still_served() {
    let harness = Harness::start();
    let response = harness.http_raw_chunked(&[
        "GET /json/list HTTP/1.1\r\nHos",
        "t: localhost\r\nUser-Agent: split\r\n",
        "\r\n",
    ]);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "a split header block must still be served, got:\n{response}"
    );
}

#[test]
fn a_non_loopback_host_header_is_refused() {
    let harness = Harness::start();

    // DNS rebinding: the endpoint binds loopback, but a browser will resolve an
    // attacker's domain to 127.0.0.1 and then let a hostile page talk to it.
    // Chrome checks `Host` for exactly this reason. Without the check, any web
    // page could read `/json/version` — which publishes the token — and then
    // drive the protocol.
    let response = harness.http_get_with_host("/json/version", "attacker.example");
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "a rebinding Host must be refused, got:\n{response}"
    );

    // Loopback spellings all pass, with or without a port.
    for host in [
        "127.0.0.1",
        "localhost",
        "[::1]",
        "127.0.0.1:1",
        "localhost:9222",
    ] {
        let response = harness.http_get_with_host("/json/version", host);
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "Host {host} should be accepted, got:\n{response}"
        );
    }
}

#[test]
fn a_missing_host_header_is_refused() {
    let harness = Harness::start();
    let response =
        harness.http_raw_chunked(&["GET /json/version HTTP/1.1\r\nConnection: close\r\n\r\n"]);
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "HTTP/1.1 requires a Host; a request without one must not be served:\n{response}"
    );
}

#[test]
fn a_non_ascii_session_id_does_not_kill_the_connection() {
    let harness = Harness::start();
    let mut client = harness.client();

    // The lane thread's name used to be built with a *byte* slice of the
    // client-supplied `sessionId`. A multi-byte character straddling byte 8 is
    // then a panic inside the read loop, which takes the whole connection down
    // — reachable by anyone who can open a socket.
    for session in ["aαααααααα", "日本語テキスト", "🙂🙂🙂🙂🙂🙂🙂🙂🙂"]
    {
        let error = client
            .try_call_on(session, "Target.getTargets", json!({}))
            .expect_err("no such session");
        assert_eq!(error["code"], -32000);
    }

    // Still alive.
    let version = client.call("Browser.getVersion", json!({}));
    assert_eq!(version["protocolVersion"], "1.3");
}

/// The observable half of the lane-leak fix. That no *thread* is spawned is
/// pinned directly by `session::tests::an_unknown_session_spawns_no_lane`,
/// which can see the lane table; this checks the connection stays healthy
/// across a flood of invented ids.
#[test]
fn a_flood_of_unknown_sessions_leaves_the_connection_usable() {
    let harness = Harness::start();
    let mut client = harness.client();

    for index in 0..200 {
        let session = format!("{index:032x}");
        let error = client
            .try_call_on(&session, "Target.getTargets", json!({}))
            .expect_err("no such session");
        assert_eq!(error["code"], -32000);
    }

    let version = client.call("Browser.getVersion", json!({}));
    assert_eq!(version["protocolVersion"], "1.3");
}

#[test]
fn creating_a_target_with_an_empty_url_yields_about_blank() {
    let harness = Harness::start();
    let mut client = harness.client();

    // `{"url": ""}` counted as blank but was then echoed verbatim into the
    // page's document URL, leaving `location.href` as the empty string.
    let created = client.call("Target.createTarget", json!({ "url": "" }));
    let targets = client.call("Target.getTargets", json!({}));
    let info = targets["targetInfos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|info| info["targetId"] == created["targetId"])
        .expect("target missing");
    assert_eq!(info["url"], "about:blank");
}

#[test]
fn create_target_answers_promptly_for_a_page_that_waits_for_the_debugger() {
    let harness = Harness::start();
    let mut client = harness.client();

    // With `waitForDebuggerOnStart` the page is created **suspended**, and an
    // ordinary job on a suspended page is deferred until `resume()` — which the
    // driver can only send after this command answers. Probing the page's URL
    // here was therefore a deadlock that resolved only when the 30 s command
    // timeout expired, well past any driver's own timeout.
    client.call(
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
    );

    let started = std::time::Instant::now();
    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let elapsed = started.elapsed();

    assert!(!created["targetId"].as_str().unwrap().is_empty());
    // Well under the 30 s command timeout, which is the deadlock this guards:
    // the bug made `createTarget` wait it out in full. A tighter wall-clock
    // bound is not a stronger test, only a flakier one — this runs alongside
    // the rest of the suite on a machine that may be building at the same time,
    // and 5 s was reachable by load alone.
    assert!(
        elapsed < Duration::from_secs(20),
        "createTarget took {elapsed:?}; it must not block on the suspended page"
    );

    // And the target is reported with its real URL, not an empty one.
    let event = client.await_event("Target.attachedToTarget");
    assert_eq!(event["params"]["targetInfo"]["url"], "about:blank");
    assert_eq!(event["params"]["waitingForDebugger"], true);
}

#[test]
fn shutdown_immediately_after_start_is_not_lost() {
    // `Notify::notify_waiters` wakes only waiters already registered. The accept
    // loop registers inside its `select!`, so a stop raised before it got there
    // — or between two iterations — used to vanish, leaving `wait()` blocked
    // until some unrelated connection happened to wake the loop.
    let browser = Browser::new(BrowserOptions::default()).expect("browser");
    let server = CdpServer::start(browser.clone(), ServerOptions::default()).expect("server");
    server.shutdown();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        server.wait();
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("shutdown() raced with start and was lost: wait() never returned");
    browser.close();
}

#[test]
fn a_request_body_cannot_forge_the_host_header() {
    let harness = Harness::start();

    // `read_head` used to parse every line of whatever arrived in the same
    // `read()`, head and body alike, with a last-wins `Host`. A page on a
    // rebound domain could then send a CORS-simple POST whose *body* carried
    // `Host: 127.0.0.1` and walk straight through the loopback check.
    let body = "\r\nHost: 127.0.0.1\r\n";
    let response = harness.http_raw_chunked(&[&format!(
        "POST /json/version HTTP/1.1\r\nHost: attacker.example\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )]);
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "a body must not be able to forge Host:\n{response}"
    );
}

#[test]
fn a_duplicate_host_header_is_refused() {
    let harness = Harness::start();
    // Two `Host`s is a smuggling shape, not a client quirk: it exists to make
    // two parsers disagree about which one counts.
    let response = harness.http_raw_chunked(&[
        "GET /json/version HTTP/1.1\r\nHost: attacker.example\r\nHost: 127.0.0.1\r\n\
         Connection: close\r\n\r\n",
    ]);
    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "a duplicate Host must not be served:\n{response}"
    );
}

#[test]
fn a_websocket_upgrade_from_a_web_origin_is_refused() {
    let harness = Harness::start();
    let url = harness.server.browser_ws_url();
    let path = url.rsplit_once("127.0.0.1").map(|(_, rest)| rest).unwrap();
    let path = path
        .split_once('/')
        .map(|(_, rest)| format!("/{rest}"))
        .unwrap();

    // A browser applies neither CORS nor a cross-origin block to
    // `new WebSocket(...)`, and such a request carries a loopback `Host` with
    // no rebinding at all — so `Origin` is the only thing separating a page
    // from the protocol. Chrome refuses every origin by default too.
    let response = harness.http_raw_chunked(&[&format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://evil.example\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    )]);
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "an upgrade carrying Origin must be refused:\n{response}"
    );
}

#[test]
fn a_dialog_can_be_answered_on_the_session_that_is_navigating() {
    let fixtures = Fixtures::start(vec![(
        "/a",
        "<!doctype html><title>D</title><script>document.title = String(confirm('ok?'));</script>",
    )]);
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    // Two commands in flight on **one** connection, which is the only shape
    // that shows the bug: the page parks inside `confirm()` while
    // `Page.navigate` still occupies the session lane, so the answer used to
    // queue behind the very command it must unblock. The dialog then timed out,
    // auto-dismissed, and the answer arrived to find nothing showing.
    let navigate = client.dispatch(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );
    client.await_event("Page.javascriptDialogOpening");

    let started = std::time::Instant::now();
    let answer = client.dispatch(
        &session,
        "Page.handleJavaScriptDialog",
        json!({ "accept": true }),
    );
    let result = client.collect(answer);
    let elapsed = started.elapsed();

    assert!(
        result.is_ok(),
        "answering on the navigating session failed: {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the answer took {elapsed:?}; it must not queue behind the navigation"
    );

    // And the navigation completes, with the page having seen `true`.
    assert!(client.collect(navigate).is_ok());
    assert_eq!(
        client.call_on(
            &session,
            "Runtime.evaluate",
            json!({ "expression": "document.title", "returnByValue": true }),
        )["result"]["value"],
        "true"
    );
}

#[test]
fn each_isolated_world_gets_a_context_id_of_its_own() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    // A driver keys its context map by id, so two worlds sharing one collide
    // exactly as a world colliding with the main one does — the second
    // registration silently wins and the first world's events are dropped.
    let first = client.call_on(
        &session,
        "Page.createIsolatedWorld",
        json!({ "worldName": "one" }),
    );
    let second = client.call_on(
        &session,
        "Page.createIsolatedWorld",
        json!({ "worldName": "two" }),
    );
    assert_ne!(first["executionContextId"], second["executionContextId"]);

    // Asking twice for the same world is idempotent within a document
    // (ADR-0033 D9): Chrome mints a fresh context per call, but a driver calls
    // this once per navigation and the protocol offers no way to destroy the
    // surplus, so minting would leak a context per navigation.
    let again = client.call_on(
        &session,
        "Page.createIsolatedWorld",
        json!({ "worldName": "one" }),
    );
    assert_eq!(first["executionContextId"], again["executionContextId"]);

    // Neither collides with the main world's context.
    let main = client.call_on(&session, "Runtime.evaluate", json!({ "expression": "1" }));
    let _ = main;
    let contexts = [
        first["executionContextId"].as_i64().unwrap(),
        second["executionContextId"].as_i64().unwrap(),
    ];
    assert!(
        contexts.iter().all(|id| *id > 1),
        "an isolated world must not report the main context's id: {contexts:?}"
    );
}

/// A commit destroys every isolated world and rebuilds it under the same name
/// against a fresh global — so the same name yields a **new** id (ADR-0033 D9).
#[test]
fn a_world_gets_a_new_context_id_after_a_navigation() {
    let fixtures = Fixtures::start(vec![("/a", "<!doctype html><title>A</title>")]);
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    let before = client.call_on(
        &session,
        "Page.createIsolatedWorld",
        json!({ "worldName": "utility" }),
    );
    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({
            "expression": "globalThis.__stale = 1",
            "contextId": before["executionContextId"],
        }),
    );

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    let after = client.call_on(
        &session,
        "Page.createIsolatedWorld",
        json!({ "worldName": "utility" }),
    );
    assert_ne!(
        before["executionContextId"], after["executionContextId"],
        "a rebuilt world must report a new id, or a driver keeps using a dead one"
    );

    // The global really is fresh — not the old one under a new number.
    let stale = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({
            "expression": "typeof globalThis.__stale",
            "contextId": after["executionContextId"],
        }),
    );
    assert_eq!(stale["result"]["value"], "undefined");
}

/// The world registry is the **page's**, not the session's, so a second session
/// attached to the same target sees the worlds the first one created.
#[test]
fn two_sessions_on_one_target_see_the_same_worlds() {
    let harness = Harness::start();
    let (mut client, first_session, target) = harness.attached();

    let created = client.call_on(
        &first_session,
        "Page.createIsolatedWorld",
        json!({ "worldName": "shared" }),
    );
    let context_id = created["executionContextId"].clone();

    // A separate connection and session onto the same target.
    let (mut other, second_session) = harness.attach_existing(&target);
    other.call_on(
        &second_session,
        "Runtime.evaluate",
        json!({ "expression": "globalThis.__seen = 'yes'", "contextId": context_id }),
    );
    let read_back = client.call_on(
        &first_session,
        "Runtime.evaluate",
        json!({ "expression": "__seen", "contextId": context_id }),
    );
    assert_eq!(read_back["result"]["value"], "yes");
}

#[test]
fn an_unserializable_argument_arrives_as_a_primitive_not_a_string() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    // `NaN` has no JSON spelling, so the obvious encoding — a JSON string —
    // delivers the *string* `"NaN"` to the page. Every driver sends this form
    // for `page.evaluate(x => …, NaN)`.
    for (literal, expected) in [
        ("NaN", "number:NaN"),
        ("Infinity", "number:Infinity"),
        ("-Infinity", "number:-Infinity"),
    ] {
        let result = client.call_on(
            &session,
            "Runtime.callFunctionOn",
            json!({
                "functionDeclaration": "(x) => typeof x + ':' + String(x)",
                "arguments": [{ "unserializableValue": literal }],
                "returnByValue": true,
            }),
        );
        assert_eq!(result["result"]["value"], expected, "{literal}");
    }

    // Arbitrary source is refused: the member carries data, not code.
    let error = client
        .try_call_on(
            &session,
            "Runtime.callFunctionOn",
            json!({
                "functionDeclaration": "(x) => x",
                "arguments": [{ "unserializableValue": "globalThis.__pwned = 1" }],
            }),
        )
        .expect_err("arbitrary source must be refused");
    assert_eq!(error["code"], -32602);
}

#[test]
fn a_screenshot_honours_the_emulated_device_scale_factor() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(
        &session,
        "Emulation.setDeviceMetricsOverride",
        json!({ "width": 400, "height": 300, "deviceScaleFactor": 2, "mobile": false }),
    );

    let shot = client.call_on(&session, "Page.captureScreenshot", json!({}));
    let png = decode_base64(shot["data"].as_str().unwrap());
    // PNG IHDR: width and height are big-endian u32 at bytes 16..24.
    let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    assert_eq!(
        (width, height),
        (800, 600),
        "deviceScaleFactor must reach the rasterizer"
    );
}

/// Minimal base64 decoder, for reading a PNG header back out of a result.
fn decode_base64(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes().filter(|b| *b != b'=') {
        let Some(index) = ALPHABET.iter().position(|c| *c == byte) else {
            continue;
        };
        acc = (acc << 6) | index as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[test]
fn an_empty_response_body_still_reports_loading_finished() {
    // A 204/HEAD/bodyless 404 produces no `Chunk` at all, so `Done` is the only
    // place it can be closed out. Without it a driver counts the request in
    // flight forever and every `networkidle` wait hangs.
    let fixtures = Fixtures::start(vec![(
        "/a",
        "<!doctype html><title>A</title><script src=/empty.js></script>",
    )]);
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(&session, "Network.enable", json!({}));

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );
    let events = client.drain_events(Duration::from_millis(800));

    let started: Vec<&str> = events
        .iter()
        .filter(|e| e["method"] == "Network.requestWillBeSent")
        .filter_map(|e| e["params"]["requestId"].as_str())
        .collect();
    let finished: Vec<&str> = events
        .iter()
        .filter(|e| {
            e["method"] == "Network.loadingFinished" || e["method"] == "Network.loadingFailed"
        })
        .filter_map(|e| e["params"]["requestId"].as_str())
        .collect();
    for id in &started {
        assert!(
            finished.contains(id),
            "request {id} never reported a terminal event; started={started:?} finished={finished:?}"
        );
    }
}

#[test]
fn json_new_refuses_a_method_a_web_page_can_send() {
    let harness = Harness::start();

    // `GET` and `POST` are CORS-*simple*: any page on the open web can issue one
    // cross-origin with no preflight and no permission, and an `<img src=…>`
    // does not even carry an `Origin` to refuse. So a hostile page could open a
    // target and navigate it from the operator's network position — repeatable
    // in a loop. `PUT` cannot be sent without a preflight this endpoint never
    // answers, which is why Chrome moved `/json/new` to it.
    for method in ["GET", "POST"] {
        let response = harness.http_request(method, "/json/new?url=about:blank");
        assert!(
            response.starts_with("HTTP/1.1 405"),
            "{method} /json/new must be refused:\n{response}"
        );
    }

    let mut client = harness.client();
    let targets = client.call("Target.getTargets", json!({}));
    assert!(
        targets["targetInfos"].as_array().unwrap().is_empty(),
        "a refused /json/new must not have created a target: {targets}"
    );
}

#[test]
fn a_cross_origin_http_request_is_refused() {
    let harness = Harness::start();
    // The reply to a cross-origin `fetch` is unreadable, but that only protects
    // the *response*. `/json/new` acts, and an effect needs no readable answer
    // to be worth having — so `Origin` disqualifies a `/json/*` request exactly
    // as it disqualifies an upgrade.
    let response = harness.http_raw_chunked(&[
        "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://evil.example\r\n\
         Connection: close\r\n\r\n",
    ]);
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "a request from a web origin must be refused:\n{response}"
    );
}

#[test]
fn a_driver_created_context_inherits_the_default_configuration() {
    let fixtures = Fixtures::start(vec![(
        "/a",
        "<!doctype html><title>D</title><script>confirm('ok?');</script>",
    )]);
    let harness = Harness::start();
    let mut client = harness.client();

    // `createBrowserContext` used to build a context from `ContextOptions
    // ::default()`, discarding everything the operator configured on `serve` —
    // the viewport, and `DialogPolicy::Ask`, without which the page answers its
    // own dialogs and `page.on('dialog', …)` can never fire. An incognito
    // context is meant to be *isolated*, not differently configured.
    let context = client.call("Target.createBrowserContext", json!({}));
    let context_id = context["browserContextId"].as_str().expect("context id");
    let created = client.call(
        "Target.createTarget",
        json!({ "url": "about:blank", "browserContextId": context_id }),
    );
    let target = created["targetId"].as_str().expect("targetId").to_owned();
    let attached = client.call(
        "Target.attachToTarget",
        json!({ "targetId": &target, "flatten": true }),
    );
    let session = attached["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_owned();
    client.call_on(&session, "Page.enable", json!({}));

    // Dispatched, not awaited: the page parks inside `confirm()` and the
    // navigation only answers once the dialog does.
    let navigate = client.dispatch(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );
    assert!(
        client
            .try_await_event("Page.javascriptDialogOpening")
            .is_some(),
        "a page in a driver-created context must raise its dialogs to the driver"
    );
    // And answerable: under the stock `DialogPolicy::Dismiss` the page answers
    // itself, so this is refused with "No dialog handler is installed".
    assert!(
        client
            .try_call_on(
                &session,
                "Page.handleJavaScriptDialog",
                json!({ "accept": true }),
            )
            .is_ok(),
        "the dialog must be answerable by the driver"
    );
    assert!(client.collect(navigate).is_ok());
}

#[test]
fn a_symbol_is_described_rather_than_reported_as_out_of_handles() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(&session, "Runtime.enable", json!({}));

    // A symbol is a primitive here: it never gets an `objectId`. The
    // out-of-handles check read "handle-shaped but empty" off the *absence* of
    // one, so `page.evaluateHandle(() => Symbol('x'))` came back as a full
    // object table rather than a symbol.
    let result = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "Symbol('x')" }),
    );
    assert!(
        result["exceptionDetails"].is_null(),
        "describing a symbol must not fail: {result}"
    );
    assert_eq!(result["result"]["type"], "symbol");
    assert_eq!(result["result"]["description"], "Symbol(x)");
}

#[test]
fn a_slow_target_cannot_strand_another_targets_dialog() {
    let fixtures = Fixtures::start(vec![(
        "/dialog",
        "<!doctype html><title>D</title><script>confirm('ok?');</script>",
    )]);
    let harness = Harness::start();
    let (mut client, slow_session, _) = harness.attached();

    // A second target on the **same connection**, which is what makes the
    // shared priority lane observable.
    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let attached = client.call(
        "Target.attachToTarget",
        json!({ "targetId": created["targetId"], "flatten": true }),
    );
    let dialog_session = attached["sessionId"].as_str().unwrap().to_owned();
    client.call_on(&dialog_session, "Page.enable", json!({}));

    // `Page.stopLoading` used to ride the priority lane. It reads like a fit —
    // it exists to interrupt a load — but the page thread is inside a *blocking*
    // document fetch for the whole of a slow load and services no job of any
    // kind, so the command sat on the one lane every session shares. The other
    // target's dialog answer then queued behind an unrelated page's fetch and
    // the dialog auto-dismissed on timeout.
    let slow = client.dispatch(
        &slow_session,
        "Page.navigate",
        json!({ "url": fixtures.url("/slow-3000") }),
    );
    std::thread::sleep(Duration::from_millis(200));
    let stop = client.dispatch(&slow_session, "Page.stopLoading", json!({}));

    let opening = client.dispatch(
        &dialog_session,
        "Page.navigate",
        json!({ "url": fixtures.url("/dialog") }),
    );
    client.await_event("Page.javascriptDialogOpening");

    let started = std::time::Instant::now();
    let answer = client.dispatch(
        &dialog_session,
        "Page.handleJavaScriptDialog",
        json!({ "accept": true }),
    );
    let result = client.collect(answer);
    let elapsed = started.elapsed();

    assert!(result.is_ok(), "answering the dialog failed: {result:?}");
    assert!(
        elapsed < Duration::from_millis(1500),
        "the dialog answer took {elapsed:?}; another target's load must not delay it"
    );

    // The three commands still in flight are left to finish on their own: their
    // replies arrive in completion order, and `collect` discards every response
    // it is not waiting for, so asking for them in dispatch order would drop one
    // and then block for it.
    let _ = (opening, stop, slow);
}
