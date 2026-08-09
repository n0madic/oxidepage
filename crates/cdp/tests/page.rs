//! The `Page` domain: navigation, lifecycle events, history, capture, dialogs.

mod common;

use std::time::Duration;

use common::{Fixtures, Harness};
use serde_json::json;

fn doc(title: &str, body: &str) -> String {
    format!("<!doctype html><meta charset=utf-8><title>{title}</title>{body}")
}

#[test]
fn navigate_answers_with_the_frame_and_loader() {
    let fixtures = Fixtures::start(vec![("/a", "<title>A</title>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();

    let result = client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );
    assert!(!result["frameId"].as_str().unwrap().is_empty());
    assert!(!result["loaderId"].as_str().unwrap().is_empty());
    assert!(
        result.get("errorText").is_none(),
        "a successful navigation must carry no errorText: {result}"
    );
}

#[test]
fn a_failed_navigation_answers_with_error_text_not_a_protocol_error() {
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();

    // Chrome reports the failure in the *result*, and Puppeteer turns that into
    // a rejected `page.goto`. Failing the command instead loses the URL and
    // reads to a driver as a broken browser rather than a broken page.
    let result = client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": "http://127.0.0.1:1/nothing-listens-here" }),
    );
    assert!(
        result["errorText"].as_str().is_some_and(|t| !t.is_empty()),
        "expected errorText, got {result}"
    );
}

#[test]
fn a_navigation_produces_the_lifecycle_sequence_a_driver_waits_on() {
    let fixtures = Fixtures::start(vec![("/a", &doc("A", "<p>hello</p>"))]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    // The four names Puppeteer's LifecycleWatcher matches. A typo in any of
    // them makes `page.goto({waitUntil})` hang until its own timeout.
    let mut seen: Vec<String> = Vec::new();
    for event in client.drain_events(Duration::from_millis(600)) {
        if event["method"] == "Page.lifecycleEvent" {
            seen.push(event["params"]["name"].as_str().unwrap_or("").to_owned());
        }
    }
    for name in ["init", "DOMContentLoaded", "load"] {
        assert!(
            seen.iter().any(|got| got == name),
            "missing lifecycle {name}; saw {seen:?}"
        );
    }
}

#[test]
fn a_navigation_reports_the_frame_milestones() {
    let fixtures = Fixtures::start(vec![("/a", &doc("A", ""))]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    let events = client.drain_events(Duration::from_millis(600));
    let names: Vec<&str> = events.iter().filter_map(|e| e["method"].as_str()).collect();
    for expected in [
        "Page.frameStartedLoading",
        "Page.frameNavigated",
        "Page.domContentEventFired",
        "Page.loadEventFired",
    ] {
        assert!(
            names.contains(&expected),
            "missing {expected}; saw {names:?}"
        );
    }

    let navigated = events
        .iter()
        .find(|e| e["method"] == "Page.frameNavigated")
        .expect("frameNavigated");
    assert_eq!(navigated["params"]["frame"]["url"], fixtures.url("/a"));
    assert!(
        !navigated["params"]["frame"]["loaderId"]
            .as_str()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn no_lifecycle_events_reach_a_session_that_did_not_enable_page() {
    let fixtures = Fixtures::start(vec![("/a", &doc("A", ""))]);
    let harness = Harness::start();
    let mut client = harness.client();
    let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));
    let attached = client.call(
        "Target.attachToTarget",
        json!({ "targetId": created["targetId"], "flatten": true }),
    );
    let session = attached["sessionId"].as_str().unwrap().to_owned();
    // Deliberately no `Page.enable`.

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );
    let events = client.drain_events(Duration::from_millis(400));
    assert!(
        !events
            .iter()
            .any(|e| e["method"].as_str().is_some_and(|m| m.starts_with("Page."))),
        "Page events leaked to a session that never enabled the domain: {events:?}"
    );
}

#[test]
fn a_cross_document_navigation_mints_a_new_loader() {
    let fixtures = Fixtures::start(vec![("/a", &doc("A", "")), ("/b", &doc("B", ""))]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();

    let first = client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );
    let second = client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/b") }),
    );

    // The loaderId is how a driver tells a new document from a same-document
    // change. Reusing it would make the two indistinguishable.
    assert_ne!(
        first["loaderId"], second["loaderId"],
        "a second document must get a new loaderId"
    );
    assert_eq!(first["frameId"], second["frameId"], "the frame is the same");
}

/// The `init` lifecycle event must carry the loader of the document being
/// *loaded*, and a navigation that fails must not spend the next one's id.
///
/// `Page.lifecycleEvent { name: "init" }` is the only event that moves
/// Puppeteer's `frame._loaderId`, and `LifecycleWatcher` resolves a navigation
/// only once that value has changed. Emitting the *outgoing* loader made
/// `page.goto()` hang for the full 30 s after any navigation that had failed
/// without committing — because the committed loader had not moved either.
#[test]
fn each_navigation_reports_a_fresh_loader_on_init() {
    let fixtures = Fixtures::start(vec![("/a", &doc("A", "")), ("/b", &doc("B", ""))]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(
        &session,
        "Page.setLifecycleEventsEnabled",
        json!({ "enabled": true }),
    );

    let init_loader = |client: &mut common::Client| loop {
        let event = client.await_event("Page.lifecycleEvent");
        if event["params"]["name"] == "init" {
            return event["params"]["loaderId"].as_str().unwrap().to_owned();
        }
    };

    // The target's opening `about:blank` navigation can land either side of the
    // command that enabled lifecycle events, so start from a known-empty
    // baseline rather than counting from the top of the stream.
    client.forget_events(std::time::Duration::from_millis(300));
    let before =
        client.call_on(&session, "Page.getFrameTree", json!({}))["frameTree"]["frame"]["loaderId"]
            .as_str()
            .expect("a loader")
            .to_owned();

    // A navigation that genuinely fails — nothing is listening on port 1, so it
    // never commits. (A 404 would *commit*, which is the opposite of the case
    // under test.)
    let failed = client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": "http://127.0.0.1:1/unreachable" }),
    );
    assert!(
        failed.get("errorText").is_some(),
        "the fixture URL must actually fail: {failed}"
    );
    let failed_init = init_loader(&mut client);

    // The *committed* loader has not moved: a navigation that never produced a
    // document must not retire the one that is still on screen.
    let after_failure =
        client.call_on(&session, "Page.getFrameTree", json!({}))["frameTree"]["frame"]["loaderId"]
            .as_str()
            .expect("a loader")
            .to_owned();
    assert_eq!(
        before, after_failure,
        "a failed navigation must not commit a loader"
    );

    let ok = client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );
    let ok_init = init_loader(&mut client);

    assert_ne!(
        failed_init, ok_init,
        "a navigation after a failed one must still announce a fresh loader"
    );
    assert_eq!(
        ok_init, ok["loaderId"],
        "the loader `init` announced is the one the commit adopted"
    );
    assert_ne!(
        before, ok["loaderId"],
        "and the committed loader moved once a document really arrived"
    );
}

/// `Page.navigatedWithinDocument` must say *which kind* of same-document
/// navigation it was: Chrome always sends `navigationType`, and a driver
/// branches on it rather than re-deriving it from the URL.
#[test]
fn a_fragment_navigation_reports_its_navigation_type() {
    let fixtures = Fixtures::start(vec![("/a", &doc("A", ""))]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a#one") }),
    );

    let event = client.await_event("Page.navigatedWithinDocument");
    assert_eq!(event["params"]["url"], fixtures.url("/a#one"));
    assert_eq!(
        event["params"]["navigationType"], "fragment",
        "only the fragment moved: {event}"
    );
}

/// The other branch: a same-document *traversal* back to an entry a
/// `pushState` displaced. The URL difference is not a fragment, so it is not a
/// fragment navigation — which is the whole distinction `navigationType` draws.
#[test]
fn a_history_traversal_reports_a_history_api_navigation_type() {
    let fixtures = Fixtures::start(vec![("/a", &doc("A", ""))]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );
    // `pushState` moves the document URL with no navigation milestone at all,
    // so this produces no event of its own — the traversal below is the first
    // same-document navigation the registry sees.
    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "history.pushState(null, '', '/pushed')" }),
    );

    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "history.back()" }),
    );

    let event = client.await_event("Page.navigatedWithinDocument");
    assert_eq!(event["params"]["url"], fixtures.url("/a"));
    assert_eq!(
        event["params"]["navigationType"], "historyApi",
        "a traversal that is not a fragment change: {event}"
    );
}

#[test]
fn get_frame_tree_reports_the_current_document() {
    let fixtures = Fixtures::start(vec![("/a", &doc("A", ""))]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    let tree = client.call_on(&session, "Page.getFrameTree", json!({}));
    let frame = &tree["frameTree"]["frame"];
    assert_eq!(frame["url"], fixtures.url("/a"));
    assert!(!frame["id"].as_str().unwrap().is_empty());
    assert_eq!(
        frame["securityOrigin"],
        format!(
            "http://127.0.0.1:{}",
            fixtures
                .url("/")
                .trim_start_matches("http://127.0.0.1:")
                .trim_end_matches('/')
        )
    );
    // Present and empty rather than absent — a driver iterates it blindly.
    assert_eq!(tree["frameTree"]["childFrames"], json!([]));
}

#[test]
fn history_is_reported_and_traversable() {
    let fixtures = Fixtures::start(vec![("/a", &doc("A", "")), ("/b", &doc("B", ""))]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/b") }),
    );

    let history = client.call_on(&session, "Page.getNavigationHistory", json!({}));
    let entries = history["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "history: {history}");
    assert_eq!(entries[0]["url"], fixtures.url("/a"));
    assert_eq!(entries[1]["url"], fixtures.url("/b"));
    assert_eq!(history["currentIndex"], 1);

    // Back to the first entry.
    client.call_on(
        &session,
        "Page.navigateToHistoryEntry",
        json!({ "entryId": 0 }),
    );
    let history = client.call_on(&session, "Page.getNavigationHistory", json!({}));
    assert_eq!(history["currentIndex"], 0);
    assert_eq!(
        client.call_on(&session, "Page.getFrameTree", json!({}))["frameTree"]["frame"]["url"],
        fixtures.url("/a")
    );
}

#[test]
fn reload_keeps_one_history_entry() {
    let fixtures = Fixtures::start(vec![("/a", &doc("A", ""))]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    client.call_on(&session, "Page.reload", json!({}));

    // A reload replaces its entry; it does not add to the back stack.
    let history = client.call_on(&session, "Page.getNavigationHistory", json!({}));
    assert_eq!(history["entries"].as_array().unwrap().len(), 1);
    assert_eq!(history["currentIndex"], 0);
}

#[test]
fn stop_loading_answers() {
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    // Nothing queued, so it drops nothing — but the command must exist and
    // answer, because a driver sends it between navigations.
    client.call_on(&session, "Page.stopLoading", json!({}));
}

#[test]
fn capture_screenshot_returns_png_bytes() {
    let fixtures = Fixtures::start(vec![(
        "/a",
        &doc("A", "<body style='background:#0f0'><p>hi</p>"),
    )]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    let shot = client.call_on(&session, "Page.captureScreenshot", json!({}));
    let data = shot["data"].as_str().expect("data");
    assert!(!data.is_empty());
    // The PNG signature, base64-encoded. Catches an empty or mis-encoded
    // payload, which a length check alone would not.
    assert!(
        data.starts_with("iVBORw0KGgo"),
        "not a PNG: {}",
        &data[..data.len().min(32)]
    );
}

#[test]
fn capture_screenshot_honors_the_format_and_refuses_what_is_absent() {
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();

    let jpeg = client.call_on(
        &session,
        "Page.captureScreenshot",
        json!({ "format": "jpeg", "quality": 60 }),
    );
    let data = jpeg["data"].as_str().unwrap();
    // JPEG's SOI marker, `FF D8 FF`, base64-encoded.
    assert!(
        data.starts_with("/9j/"),
        "not a JPEG: {}",
        &data[..16.min(data.len())]
    );

    // WebP encoding is a documented non-goal (ADR-0026): refused, not silently
    // served as PNG under a webp filename.
    let error = client
        .try_call_on(
            &session,
            "Page.captureScreenshot",
            json!({ "format": "webp" }),
        )
        .expect_err("webp is not implemented");
    assert_eq!(error["code"], -32602);
}

#[test]
fn print_to_pdf_returns_a_pdf() {
    let fixtures = Fixtures::start(vec![("/a", &doc("A", "<p>printable</p>"))]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    let pdf = client.call_on(&session, "Page.printToPDF", json!({}));
    let data = pdf["data"].as_str().expect("data");
    // "%PDF" base64-encoded.
    assert!(
        data.starts_with("JVBER"),
        "not a PDF: {}",
        &data[..16.min(data.len())]
    );
}

#[test]
fn print_to_pdf_can_hand_back_a_stream() {
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();

    // Puppeteer asks for a stream by default, so `page.pdf()` does not work
    // without this path.
    let result = client.call_on(
        &session,
        "Page.printToPDF",
        json!({ "transferMode": "ReturnAsStream" }),
    );
    let handle = result["stream"].as_str().expect("stream handle").to_owned();

    let mut pdf = Vec::new();
    loop {
        let chunk = client.call_on(&session, "IO.read", json!({ "handle": &handle }));
        assert_eq!(chunk["base64Encoded"], true);
        pdf.push(chunk["data"].as_str().unwrap().to_owned());
        if chunk["eof"] == json!(true) {
            break;
        }
    }
    assert!(pdf[0].starts_with("JVBER"), "not a PDF: {}", &pdf[0][..8]);

    client.call_on(&session, "IO.close", json!({ "handle": &handle }));
    // A handle that has been closed names nothing.
    assert!(
        client
            .try_call_on(&session, "IO.read", json!({ "handle": &handle }))
            .is_err()
    );
}

#[test]
fn print_to_pdf_refuses_an_unknown_transfer_mode() {
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    let error = client
        .try_call_on(
            &session,
            "Page.printToPDF",
            json!({ "transferMode": "ReturnAsCarrierPigeon" }),
        )
        .expect_err("unknown transfer mode");
    assert_eq!(error["code"], -32602);
}

#[test]
fn a_dialog_is_announced_and_answered_over_the_protocol() {
    let fixtures = Fixtures::start(vec![(
        "/a",
        "<!doctype html><title>D</title>\
         <script>document.title = prompt('who?', 'nobody');</script>",
    )]);
    let harness = Harness::start();
    let (mut client, session, target) = harness.attached();

    // The page parks inside `prompt()` while `Page.navigate` is still in
    // flight, so the answer cannot come from this thread — and it cannot come
    // over the command port either, because a parked page services no ordinary
    // job. That is the rendezvous channel ADR-0027 D11 exists for.
    //
    // The watcher needs *its own* session: sessions belong to a connection, so
    // handing it this connection's id would just be refused.
    let (mut watcher, watch_session) = harness.attach_existing(&target);
    let answered = std::thread::spawn(move || {
        let opening = watcher.await_event("Page.javascriptDialogOpening");
        assert_eq!(opening["params"]["type"], "prompt");
        assert_eq!(opening["params"]["message"], "who?");
        assert_eq!(opening["params"]["defaultPrompt"], "nobody");

        watcher.call_on(
            &watch_session,
            "Page.handleJavaScriptDialog",
            json!({ "accept": true, "promptText": "somebody" }),
        );
        watcher.await_event("Page.javascriptDialogClosed")
    });

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    let closed = answered.join().expect("the dialog watcher panicked");
    assert_eq!(closed["params"]["result"], true);
    assert_eq!(closed["params"]["userInput"], "somebody");
}

#[test]
fn a_dismissed_prompt_reports_no_input() {
    let fixtures = Fixtures::start(vec![(
        "/a",
        "<!doctype html><title>D</title><script>prompt('who?');</script>",
    )]);
    let harness = Harness::start();
    let (mut client, session, target) = harness.attached();
    let (mut watcher, watch_session) = harness.attach_existing(&target);

    let answered = std::thread::spawn(move || {
        watcher.await_event("Page.javascriptDialogOpening");
        watcher.call_on(
            &watch_session,
            "Page.handleJavaScriptDialog",
            json!({ "accept": false }),
        );
        watcher.await_event("Page.javascriptDialogClosed")
    });

    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/a") }),
    );

    let closed = answered.join().expect("the dialog watcher panicked");
    assert_eq!(closed["params"]["result"], false);
    assert_eq!(closed["params"]["userInput"], "");
}

#[test]
fn page_close_destroys_the_target() {
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call_on(&session, "Page.close", json!({}));

    assert_eq!(
        client.call("Target.getTargets", json!({}))["targetInfos"],
        json!([])
    );
}

#[test]
fn page_commands_require_a_session() {
    let harness = Harness::start();
    let mut client = harness.client();

    // Sent with no `sessionId` at all: there is no target to act on, and
    // guessing one would act on an arbitrary page.
    for method in ["Page.enable", "Page.getFrameTree", "Page.captureScreenshot"] {
        let error = match client.try_call(method, json!({})) {
            Err(error) => error,
            Ok(result) => panic!("{method} answered without a session: {result}"),
        };
        assert_eq!(error["code"], -32602, "{method}");
    }
}

#[test]
fn answering_a_dialog_that_is_not_showing_says_so() {
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();

    // Not `EngineError::Timeout` rendered as "Timed out waiting for the
    // target": that describes an unresponsive page and sends a driver looking
    // for the wrong bug.
    let error = client
        .try_call_on(
            &session,
            "Page.handleJavaScriptDialog",
            json!({ "accept": true }),
        )
        .expect_err("no dialog is open");
    assert_eq!(error["code"], -32000);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("No dialog is showing"),
        "unhelpful message: {error}"
    );
}

// === downloads (ADR-0032 D13) ===

#[test]
fn set_download_behavior_allow_needs_a_path() {
    // With nowhere to write, `allow` and `deny` behave identically — so
    // accepting it would tell a driver downloads were on when nothing could be
    // written.
    let harness = Harness::start();
    let (mut client, _session, _target) = harness.attached();
    let refused = client.try_call(
        "Browser.setDownloadBehavior",
        json!({ "behavior": "allow" }),
    );
    assert!(
        refused.is_err(),
        "allow with no downloadPath must be refused"
    );
}

#[test]
fn a_traversing_download_path_is_refused() {
    // The path comes off an untrusted frame. `../../etc` names a real
    // directory, and "it resolved fine" is not the question.
    let harness = Harness::start();
    let (mut client, _session, _target) = harness.attached();
    let refused = client.try_call(
        "Browser.setDownloadBehavior",
        json!({ "behavior": "allow", "downloadPath": "../../../tmp/escaped" }),
    );
    assert!(
        refused.is_err(),
        "a traversing downloadPath must be refused"
    );
}

#[test]
fn a_download_path_set_before_a_target_exists_reaches_it() {
    // A driver commonly sends `Browser.setDownloadBehavior` before it creates a
    // page. Applying it only to the pages that happen to exist would make that
    // call a silent no-op, so the setting lives on the browsing context.
    let directory =
        std::env::temp_dir().join(format!("oxidepage-cdp-early-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);

    let harness = Harness::start();
    let mut early = harness.client();
    early.call(
        "Browser.setDownloadBehavior",
        json!({ "behavior": "allow", "downloadPath": directory.to_string_lossy() }),
    );

    // Created *after* the command.
    let (mut client, session, _target) = harness.attached();
    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    client.call_on(&session, "Network.enable", json!({}));
    client.call_on(&session, "Fetch.enable", json!({}));
    let navigate = client.dispatch(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/late.csv") }),
    );
    let paused = client.await_event("Fetch.requestPaused");
    client.call_on(
        &session,
        "Fetch.fulfillRequest",
        json!({
            "requestId": paused["params"]["requestId"],
            "responseCode": 200,
            "responseHeaders": [
                { "name": "content-disposition", "value": "attachment; filename=\"late.csv\"" },
            ],
            "body": oxidepage_cdp::base64::encode(b"late"),
        }),
    );
    let _ = client.collect(navigate);

    let done = client.await_event("Page.downloadProgress");
    assert_eq!(done["params"]["state"], "inProgress");
    let done = client.await_event("Page.downloadProgress");
    assert_eq!(
        done["params"]["state"], "completed",
        "the early setting reached a page created after it"
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("late.csv")).expect("the download"),
        "late"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn an_attachment_downloads_instead_of_committing() {
    let directory = std::env::temp_dir().join(format!("oxidepage-cdp-dl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);

    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call(
        "Browser.setDownloadBehavior",
        json!({ "behavior": "allow", "downloadPath": directory.to_string_lossy() }),
    );
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );

    // Stubbed rather than served: the fixture server sends no
    // `Content-Disposition`, and `Fetch.fulfillRequest` is the shortest way to
    // produce one.
    client.call_on(&session, "Network.enable", json!({}));
    client.call_on(&session, "Fetch.enable", json!({}));
    let navigate = client.dispatch(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/report.csv") }),
    );
    let paused = client.await_event("Fetch.requestPaused");
    let request_id = paused["params"]["requestId"].as_str().unwrap().to_owned();
    client.call_on(
        &session,
        "Fetch.fulfillRequest",
        json!({
            "requestId": request_id,
            "responseCode": 200,
            "responseHeaders": [
                { "name": "content-type", "value": "text/csv" },
                { "name": "content-disposition", "value": "attachment; filename=\"report.csv\"" },
            ],
            "body": oxidepage_cdp::base64::encode(b"a,b\n1,2\n"),
        }),
    );
    let _ = client.collect(navigate);

    let begin = client.await_event("Page.downloadWillBegin");
    assert_eq!(begin["params"]["suggestedFilename"], "report.csv");
    let progress = client.await_event("Page.downloadProgress");
    assert_eq!(progress["params"]["guid"], begin["params"]["guid"]);
    assert_eq!(progress["params"]["state"], "inProgress");
    let done = client.await_event("Page.downloadProgress");
    assert_eq!(done["params"]["state"], "completed");

    // The document did not move: a download is a navigation that never commits.
    let title = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "document.title", "returnByValue": true }),
    );
    assert_eq!(title["result"]["value"], "doc");
    assert_eq!(
        std::fs::read_to_string(directory.join("report.csv")).expect("the download"),
        "a,b\n1,2\n"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// A download navigation's `init` must carry the loader that navigation minted,
/// not the loader of the document that stays.
///
/// Same contract as `each_navigation_reports_a_fresh_loader_on_init`, reached
/// through the one navigation that starts and ends back to back: nothing is
/// parsed between `Started` and the `Failed` that abandons the load, so this is
/// the tightest window in which the abandon could spend the id `init` still has
/// to carry. It holds even when event formatting is delayed by an order of
/// magnitude more than that window — checked by hand with a sleep in
/// `dispatch_page_event`, since the ordering is what the assertion is about.
#[test]
fn a_download_navigation_reports_a_fresh_loader_on_init() {
    let directory =
        std::env::temp_dir().join(format!("oxidepage-cdp-dl-init-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);

    let fixtures = Fixtures::start(vec![("/index.html", "<title>doc</title>")]);
    let harness = Harness::start();
    let (mut client, session, _target) = harness.attached();
    client.call(
        "Browser.setDownloadBehavior",
        json!({ "behavior": "allow", "downloadPath": directory.to_string_lossy() }),
    );
    client.call_on(
        &session,
        "Page.setLifecycleEventsEnabled",
        json!({ "enabled": true }),
    );
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/index.html") }),
    );

    // The committed loader, read after the document that stays has landed.
    client.forget_events(std::time::Duration::from_millis(300));
    let committed = client.call_on(&session, "Page.getFrameTree", json!({}))["frameTree"]["frame"]
        ["loaderId"]
        .as_str()
        .expect("a loader")
        .to_owned();

    client.call_on(&session, "Network.enable", json!({}));
    client.call_on(&session, "Fetch.enable", json!({}));
    let navigate = client.dispatch(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/report.csv") }),
    );
    let paused = client.await_event("Fetch.requestPaused");
    let request_id = paused["params"]["requestId"].as_str().unwrap().to_owned();
    client.call_on(
        &session,
        "Fetch.fulfillRequest",
        json!({
            "requestId": request_id,
            "responseCode": 200,
            "responseHeaders": [
                { "name": "content-type", "value": "text/csv" },
                { "name": "content-disposition", "value": "attachment; filename=\"report.csv\"" },
            ],
            "body": oxidepage_cdp::base64::encode(b"a,b\n1,2\n"),
        }),
    );
    let _ = client.collect(navigate);

    let init = loop {
        let event = client.await_event("Page.lifecycleEvent");
        if event["params"]["name"] == "init" {
            break event["params"]["loaderId"].as_str().unwrap().to_owned();
        }
    };
    assert_ne!(
        init, committed,
        "a download navigation's `init` carried the loader of the document that stayed; a driver \
         telling documents apart by loader then never sees this navigation end"
    );

    // And the document really did stay — the fresh loader is the navigation's,
    // not evidence of a commit.
    let title = client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "document.title", "returnByValue": true }),
    );
    assert_eq!(title["result"]["value"], "doc");
    let _ = std::fs::remove_dir_all(&directory);
}

/// `Page.setBypassCSP` is accepted. No CSP is enforced anywhere in the engine,
/// so there is nothing to bypass and nothing to lie about — and refusing would
/// break `browser.newContext({ bypassCSP: true })` over a capability the page
/// never had.
#[test]
fn set_bypass_csp_is_accepted() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();
    client.call_on(&session, "Page.setBypassCSP", json!({ "enabled": true }));
}

/// `Page.getFrameTree` reports the page's real browsing contexts (ADR-0035 D9).
/// The top-level frame keeps the target id — churning it would break every
/// existing driver expectation — and a nested frame gets an opaque id of its
/// own plus a `parentId` pointing back.
#[test]
fn get_frame_tree_reports_nested_frames() {
    let fixtures = Fixtures::start(vec![(
        "/frames",
        "<!doctype html><title>Frames</title>\
         <iframe srcdoc='<p>one</p>'></iframe>\
         <iframe srcdoc='<p>two</p>'></iframe>",
    )]);
    let harness = Harness::start();
    let (mut client, session, target) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/frames") }),
    );

    let tree = client.call_on(&session, "Page.getFrameTree", json!({}));
    let root = &tree["frameTree"];
    assert_eq!(
        root["frame"]["id"], target,
        "the top-level frame keeps the target id"
    );
    assert!(root["frame"].get("parentId").is_none());

    let children = root["childFrames"].as_array().expect("childFrames");
    assert_eq!(children.len(), 2, "two <iframe>s, two frames: {tree}");
    for child in children {
        let frame = &child["frame"];
        assert_eq!(frame["parentId"], target);
        assert_ne!(
            frame["id"], target,
            "a nested frame gets an id of its own: {frame}"
        );
        assert!(!frame["id"].as_str().unwrap().is_empty());
        // Its own contexts, so it iterates like any other node in the tree.
        assert!(child["childFrames"].as_array().unwrap().is_empty());
    }
    // The two frames are distinct.
    assert_ne!(children[0]["frame"]["id"], children[1]["frame"]["id"]);
}

/// A page with no `<iframe>` still answers with the shape a driver iterates:
/// one frame, an empty `childFrames`.
#[test]
fn get_frame_tree_of_a_frameless_page_has_one_frame() {
    let harness = Harness::start();
    let (mut client, session, target) = harness.attached();

    let tree = client.call_on(&session, "Page.getFrameTree", json!({}));
    assert_eq!(tree["frameTree"]["frame"]["id"], target);
    assert!(
        tree["frameTree"]["childFrames"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
