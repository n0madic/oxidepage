//! The `Input` domain over the wire.
//!
//! What is asserted here is the vocabulary translation — the bitmask, the
//! button names, which `type` maps to which synthesis — and that the effects
//! actually reach the page. The event *sequences* themselves are pinned in
//! `crates/page/tests/input.rs`; re-asserting them here would test the engine
//! twice and the protocol once.

mod common;

use common::{Fixtures, Harness};
use serde_json::json;

/// A page laid out at known coordinates, so the numbers in these tests mean
/// what they say.
const PAGE: &str = "<!doctype html><html><head><style>\
    body { margin: 0 }\
    a { position: absolute; left: 0; top: 0; width: 200px; height: 50px }\
    input { position: absolute; left: 0; top: 60px; width: 200px; height: 30px }\
    #tall { height: 4000px }\
    </style></head><body>\
    <a id=go href=\"/landed\">go</a><input id=t><div id=tall></div>\
    <script>\
      window.log = [];\
      addEventListener('mousedown', e => \
        log.push(['down', e.button, e.buttons, e.ctrlKey, e.shiftKey, e.altKey, e.metaKey] \
          .join(':')));\
    </script></body></html>";

fn started() -> (Harness, Fixtures) {
    let fixtures = Fixtures::start(vec![
        ("/", PAGE),
        ("/landed", "<!doctype html><title>landed</title>"),
    ]);
    (Harness::start(), fixtures)
}

fn eval(client: &mut common::Client, session: &str, expression: &str) -> serde_json::Value {
    client.call_on(
        session,
        "Runtime.evaluate",
        json!({ "expression": expression, "returnByValue": true }),
    )["result"]["value"]
        .clone()
}

fn mouse(client: &mut common::Client, session: &str, params: serde_json::Value) {
    client.call_on(session, "Input.dispatchMouseEvent", params);
}

/// The milestone: a click on a link navigates, and the driver hears about it.
#[test]
fn a_click_on_a_link_navigates_and_reports_it() {
    let (harness, fixtures) = started();
    let (mut client, session, _) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/") }),
    );
    // Drop the `frameNavigated` the first load produced.
    client.await_event("Page.frameNavigated");

    for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
        mouse(
            &mut client,
            &session,
            json!({ "type": kind, "x": 100, "y": 25, "button": "left", "clickCount": 1 }),
        );
    }

    let navigated = client.await_event("Page.frameNavigated");
    assert!(
        navigated["params"]["frame"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/landed"),
        "{navigated}"
    );
}

/// The bitmask and the button name, which are pure wire contracts and the two
/// things a transcription error would silently get wrong.
#[test]
fn the_modifier_mask_and_button_name_reach_the_event() {
    let (harness, fixtures) = started();
    let (mut client, session, _) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/") }),
    );

    // Alt=1, Ctrl=2, Meta=4, Shift=8 → ctrl + shift = 10.
    mouse(
        &mut client,
        &session,
        json!({
            "type": "mousePressed", "x": 100, "y": 200,
            "button": "right", "buttons": 2, "clickCount": 1, "modifiers": 10,
        }),
    );
    assert_eq!(
        eval(&mut client, &session, "window.log.join('|')"),
        "down:2:2:true:true:false:false"
    );

    // An unknown button name is a driver bug, and is reported as one.
    let error = client
        .try_call_on(
            &session,
            "Input.dispatchMouseEvent",
            json!({ "type": "mousePressed", "x": 1, "y": 1, "button": "pedal" }),
        )
        .expect_err("an unknown button must be refused");
    assert_eq!(error["code"], -32602, "{error}");

    // So is an unknown event type — it must not silently become a move.
    let error = client
        .try_call_on(
            &session,
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseHovered", "x": 1, "y": 1 }),
        )
        .expect_err("an unknown type must be refused");
    assert_eq!(error["code"], -32602, "{error}");
}

#[test]
fn key_events_type_into_a_field_and_raw_key_down_does_not() {
    let (harness, fixtures) = started();
    let (mut client, session, _) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/") }),
    );
    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "document.getElementById('t').focus()" }),
    );

    for ch in ["h", "i"] {
        client.call_on(
            &session,
            "Input.dispatchKeyEvent",
            json!({ "type": "keyDown", "key": ch, "text": ch, "code": format!("Key{}", ch.to_uppercase()) }),
        );
        client.call_on(
            &session,
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": ch }),
        );
    }
    assert_eq!(
        eval(&mut client, &session, "document.getElementById('t').value"),
        "hi"
    );

    // `insertText` is a paste, not a key press.
    client.call_on(&session, "Input.insertText", json!({ "text": "!" }));
    assert_eq!(
        eval(&mut client, &session, "document.getElementById('t').value"),
        "hi!"
    );

    // `rawKeyDown` on Backspace still runs the key's default action — that is
    // exactly what Puppeteer sends for it.
    client.call_on(
        &session,
        "Input.dispatchKeyEvent",
        json!({ "type": "rawKeyDown", "key": "Backspace", "code": "Backspace",
                "windowsVirtualKeyCode": 8 }),
    );
    assert_eq!(
        eval(&mut client, &session, "document.getElementById('t').value"),
        "hi"
    );
}

/// A driver may name only the physical key. The table resolves it, and refuses
/// rather than inventing one it does not know.
#[test]
fn a_code_without_a_key_is_resolved_or_refused() {
    let (harness, fixtures) = started();
    let (mut client, session, _) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/") }),
    );
    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "document.getElementById('t').focus()" }),
    );

    client.call_on(
        &session,
        "Input.dispatchKeyEvent",
        json!({ "type": "keyDown", "code": "KeyA" }),
    );
    // Shift=8 gives the upper-case legend of the same physical key.
    client.call_on(
        &session,
        "Input.dispatchKeyEvent",
        json!({ "type": "keyDown", "code": "KeyB", "modifiers": 8 }),
    );
    client.call_on(
        &session,
        "Input.dispatchKeyEvent",
        json!({ "type": "keyDown", "code": "Digit1", "modifiers": 8 }),
    );
    assert_eq!(
        eval(&mut client, &session, "document.getElementById('t').value"),
        "aB!"
    );

    let error = client
        .try_call_on(
            &session,
            "Input.dispatchKeyEvent",
            json!({ "type": "keyDown", "code": "NumpadDivide" }),
        )
        .expect_err("a code this keyboard has no key for must be refused");
    assert_eq!(error["code"], -32602, "{error}");

    let error = client
        .try_call_on(
            &session,
            "Input.dispatchKeyEvent",
            json!({ "type": "char" }),
        )
        .expect_err("a char event with no text must be refused");
    assert_eq!(error["code"], -32602, "{error}");
}

#[test]
fn a_wheel_event_scrolls_the_document() {
    let (harness, fixtures) = started();
    let (mut client, session, _) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/") }),
    );

    mouse(
        &mut client,
        &session,
        json!({ "type": "mouseWheel", "x": 50, "y": 300, "deltaX": 0, "deltaY": 240 }),
    );
    assert_eq!(eval(&mut client, &session, "window.scrollY"), 240.0);

    let metrics = client.call_on(&session, "Page.getLayoutMetrics", json!({}));
    assert_eq!(metrics["layoutViewport"]["pageY"], 240.0);
    assert_eq!(metrics["cssLayoutViewport"]["pageY"], 240.0);
    assert!(metrics["contentSize"]["height"].as_f64().unwrap() >= 4000.0);
}

/// The driver is authoritative about the *physical* key. The US-layout table
/// stores `Shift` once, so it can never produce `ShiftRight` on its own — and a
/// page whose shortcut handler branches on `e.code` (the recommended
/// layout-independent idiom) would never fire.
#[test]
fn the_drivers_code_reaches_the_event_and_resolves_a_key_on_its_own() {
    let (harness, fixtures) = started();
    let (mut client, session, _) = harness.attached();
    client.call_on(
        &session,
        "Page.navigate",
        json!({ "url": fixtures.url("/") }),
    );
    client.call_on(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "window.seen = []; addEventListener('keydown', \
                 e => seen.push(e.key + ':' + e.code + ':' + e.location))" }),
    );

    // Both members sent, as every driver does.
    client.call_on(
        &session,
        "Input.dispatchKeyEvent",
        json!({ "type": "rawKeyDown", "key": "Shift", "code": "ShiftRight",
                "location": 2, "windowsVirtualKeyCode": 16 }),
    );
    // `code` alone: the reverse lookup knows the right-hand twins even though
    // the forward table never emits them.
    client.call_on(
        &session,
        "Input.dispatchKeyEvent",
        json!({ "type": "rawKeyDown", "code": "ControlRight", "location": 2 }),
    );
    assert_eq!(
        eval(&mut client, &session, "window.seen.join('|')"),
        "Shift:ShiftRight:2|Control:ControlRight:2"
    );
}

/// The domains a driver may probe: the ones with no capability behind them are
/// absent, so feature detection works (P6, ADR-0031 D4).
#[test]
fn the_unimplemented_input_methods_are_absent() {
    let harness = Harness::start();
    let (mut client, session, _) = harness.attached();

    for method in [
        "Input.dispatchTouchEvent",
        "Input.dispatchDragEvent",
        "Input.setInterceptDrags",
    ] {
        let error = client.try_call_on(&session, method, json!({})).unwrap_err();
        assert_eq!(error["code"], -32601, "{method}: {error}");
    }
}
