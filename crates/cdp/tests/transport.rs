//! The transport: discovery endpoints, the path token, envelope handling.

mod common;

use common::Harness;
use serde_json::json;

#[test]
fn json_version_advertises_a_connectable_socket_url() {
    let harness = Harness::start();
    let body = harness.http_body("/json/version", "200");

    assert_eq!(body["Protocol-Version"], "1.3");
    assert!(
        body["Browser"].as_str().unwrap().starts_with("OxidePage/"),
        "unexpected Browser: {}",
        body["Browser"]
    );
    assert!(!body["User-Agent"].as_str().unwrap().is_empty());
    // The URL it advertises must be the one that actually works — a mismatch
    // here is invisible until a real driver connects.
    assert_eq!(
        body["webSocketDebuggerUrl"].as_str().unwrap(),
        harness.server.browser_ws_url()
    );
}

#[test]
fn json_list_starts_empty_and_grows_with_targets() {
    let harness = Harness::start();
    assert_eq!(harness.http_body("/json/list", "200"), json!([]));

    let mut client = harness.client();
    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let target_id = created["targetId"].as_str().unwrap().to_owned();

    let list = harness.http_body("/json/list", "200");
    let entries = list.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], target_id);
    assert_eq!(entries[0]["type"], "page");
    assert!(
        entries[0]["webSocketDebuggerUrl"]
            .as_str()
            .unwrap()
            .ends_with(&target_id)
    );
}

#[test]
fn json_new_creates_a_page() {
    let harness = Harness::start();
    let body = harness.http_body_of("PUT", "/json/new?url=about:blank", "200");
    assert_eq!(body["type"], "page");
    assert!(!body["id"].as_str().unwrap().is_empty());

    let mut client = harness.client();
    let targets = client.call("Target.getTargets", json!({}));
    assert_eq!(targets["targetInfos"].as_array().unwrap().len(), 1);
    assert_eq!(targets["targetInfos"][0]["targetId"], body["id"]);
}

#[test]
fn an_unknown_http_path_is_a_404() {
    let harness = Harness::start();
    let response = harness.http_get("/not-a-devtools-endpoint");
    assert!(
        response.starts_with("HTTP/1.1 404"),
        "expected 404, got:\n{response}"
    );
}

#[test]
fn the_websocket_path_token_is_required() {
    let harness = Harness::start();
    let url = harness.server.browser_ws_url();

    // The advertised URL works.
    let _ = harness.client();

    // A guessed one does not: the token is what keeps a local process that
    // cannot read the URL from reaching total remote control of this process.
    let forged = url
        .rsplit_once('/')
        .map(|(prefix, _)| format!("{prefix}/00000000000000000000000000000000"))
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        runtime
            .block_on(tokio_tungstenite::connect_async(&forged))
            .is_err(),
        "a wrong token must not yield a websocket"
    );
}

#[test]
fn an_unimplemented_method_answers_method_not_found() {
    let harness = Harness::start();
    let mut client = harness.client();

    // P6: an absent domain says so. A driver can branch on this; it cannot
    // branch on a stub that returns `{}`.
    let error = client
        .try_call("Debugger.enable", json!({}))
        .expect_err("Debugger is not implemented");
    assert_eq!(error["code"], -32601);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("Debugger.enable"),
        "unhelpful message: {error}"
    );

    // A known domain with an unknown member answers the same way.
    let error = client
        .try_call("Target.notAThing", json!({}))
        .expect_err("unknown member");
    assert_eq!(error["code"], -32601);
}

#[test]
fn a_malformed_frame_is_answered_rather_than_dropped() {
    let harness = Harness::start();
    let mut client = harness.client();

    // Not JSON at all: nothing to correlate, so it comes back against id 0.
    client.send_raw("}{");
    let events = client.drain_events(std::time::Duration::from_millis(300));
    assert!(
        events.is_empty(),
        "a parse error must not look like an event"
    );

    // The connection is still usable afterwards — a bad frame must not be fatal.
    let version = client.call("Browser.getVersion", json!({}));
    assert_eq!(version["protocolVersion"], "1.3");
}

#[test]
fn a_request_with_a_bad_shape_still_answers_its_own_id() {
    let harness = Harness::start();
    let mut client = harness.client();

    // `method` must be a string. The id is recovered anyway so the driver's
    // pending promise is retired instead of waiting for its own timeout.
    client.send_raw(r#"{"id":4242,"method":17}"#);
    let response = client.await_frame(4242);
    assert_eq!(response["error"]["code"], -32600);

    // The connection is still usable afterwards — a bad frame must not be fatal.
    let version = client.call("Browser.getVersion", json!({}));
    assert_eq!(version["protocolVersion"], "1.3");
}

#[test]
fn an_unknown_session_id_is_refused() {
    let harness = Harness::start();
    let mut client = harness.client();

    let error = client
        .try_call_on(
            "deadbeefdeadbeefdeadbeefdeadbeef",
            "Target.getTargets",
            json!({}),
        )
        .expect_err("session does not exist");
    assert_eq!(error["code"], -32000);
    assert!(
        error["message"].as_str().unwrap().contains("not found"),
        "unhelpful message: {error}"
    );
}

#[test]
fn browser_get_version_reports_the_real_engine() {
    let harness = Harness::start();
    let mut client = harness.client();
    let version = client.call("Browser.getVersion", json!({}));

    assert_eq!(version["protocolVersion"], "1.3");
    // Reported honestly rather than as a V8 version: a driver that branches on
    // this must not be told it is talking to V8.
    assert_eq!(version["jsVersion"], "QuickJS-NG");
    assert!(version["product"].as_str().unwrap().contains("OxidePage"));
}

#[test]
fn download_behavior_is_accepted_before_any_target_exists() {
    let harness = Harness::start();
    let mut client = harness.client();

    // Deny is the default, and it is a real behavior now: an attachment is
    // refused and recorded rather than parsed as HTML (ADR-0032 D13).
    // Accepted with **no target attached**, deliberately: a driver routinely
    // sets the behavior before it creates a page, and the setting is remembered
    // on the browsing context so it reaches the pages made afterwards.
    client.call("Browser.setDownloadBehavior", json!({ "behavior": "deny" }));
    client.call(
        "Browser.setDownloadBehavior",
        json!({ "behavior": "default" }),
    );

    // `allow` with nowhere to write is still refused: it would behave exactly
    // like `deny` while telling the driver downloads were on.
    let error = client
        .try_call(
            "Browser.setDownloadBehavior",
            json!({ "behavior": "allow" }),
        )
        .expect_err("allow with no downloadPath must be refused");
    assert_eq!(error["code"], -32602, "{error}");
}

#[test]
fn browser_close_answers_before_it_tears_the_endpoint_down() {
    let harness = Harness::start();
    let mut client = harness.client();
    client.call("Target.createTarget", json!({ "url": "about:blank" }));

    // The reply must arrive: Puppeteer waits for it before disposing its
    // transport, so closing the socket first turns every clean shutdown into a
    // protocol error.
    client.call("Browser.close", json!({}));

    // And the endpoint really does stop: a fresh connection is refused once the
    // accept loop has wound down.
    let url = harness.server.browser_ws_url().to_owned();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if runtime
            .block_on(tokio_tungstenite::connect_async(&url))
            .is_err()
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the endpoint kept accepting connections after Browser.close");
}

#[test]
fn missing_required_params_are_invalid_params() {
    let harness = Harness::start();
    let mut client = harness.client();

    let error = client
        .try_call("Target.createTarget", json!({}))
        .expect_err("url is required");
    assert_eq!(error["code"], -32602);

    // A member of the wrong type is an error too, not a silent default.
    let error = client
        .try_call("Target.setDiscoverTargets", json!({ "discover": "yes" }))
        .expect_err("discover must be a boolean");
    assert_eq!(error["code"], -32602);
}
