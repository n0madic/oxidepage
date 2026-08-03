//! Trusted input and the node surface across the command boundary (ADR-0031).
//!
//! `crates/page/tests/` already covers what an event *sequence* does; what is
//! new here is that it survives the trip through a `PageHandle`: an owned
//! [`KeyEvent`] rebuilt on the page thread, a `NodeRef` resolved inside the
//! closure that acts on it, and a click that navigates answering only once the
//! load has committed.

mod common;

use common::{spawn_server, test_options};
use oxidepage_engine::page_api::{
    KeyEvent, KeyEventKind, Modifiers, MouseEventKind, MouseInput, NodeRef, RemoteError,
    RemoteSubtype, WheelInput,
};
use oxidepage_engine::{Browser, NewPageOptions, PageHandle, WaitUntil};

fn browser() -> Browser {
    Browser::new(test_options()).expect("browser")
}

fn eval(page: &PageHandle, source: &str) -> String {
    page.eval_to_string(source)
        .expect("page answered")
        .expect("eval succeeded")
}

/// A move, press and release at one point — how a driver spells a click.
fn click_at(page: &PageHandle, x: f32, y: f32) {
    for kind in [
        MouseEventKind::Move,
        MouseEventKind::Down,
        MouseEventKind::Up,
    ] {
        page.dispatch_mouse(MouseInput {
            kind,
            x,
            y,
            button: 0,
            buttons: u16::from(kind == MouseEventKind::Down),
            modifiers: Modifiers::default(),
            click_count: 1,
        })
        .expect("page answered");
    }
}

fn press(page: &PageHandle, key: &str) {
    for kind in [KeyEventKind::Down, KeyEventKind::Up] {
        page.dispatch_key(KeyEvent {
            kind,
            key: key.to_owned(),
            modifiers: Modifiers::default(),
            repeat: false,
            text: None,
            code: None,
            location: 0,
        })
        .expect("page answered");
    }
}

/// The milestone this stage exists for: a click that follows a link answers
/// only after the load has committed, so a driver that clicks and then reads
/// the URL sees the new one.
#[test]
fn a_click_that_navigates_completes_before_the_command_answers() {
    let server = spawn_server();
    let browser = browser();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    page.navigate(&server.url("/interact"), WaitUntil::Load)
        .unwrap()
        .unwrap();

    click_at(&page, 100.0, 25.0);

    assert!(
        page.url().unwrap().ends_with("/landed"),
        "got {}",
        page.url().unwrap()
    );
    browser.close();
}

/// The owned key event survives the trip: the borrowed `KeyInput` is rebuilt on
/// the page thread from data the closure owns.
#[test]
fn typing_over_the_command_boundary_edits_the_value() {
    let server = spawn_server();
    let browser = browser();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    page.navigate(&server.url("/interact"), WaitUntil::Load)
        .unwrap()
        .unwrap();

    click_at(&page, 100.0, 75.0);
    assert_eq!(eval(&page, "document.activeElement.id"), "t");
    for ch in "hi".chars() {
        press(&page, &ch.to_string());
    }
    assert_eq!(eval(&page, "document.getElementById('t').value"), "hi");

    // A driver's own text wins over the US-layout table.
    page.dispatch_key(KeyEvent {
        kind: KeyEventKind::Down,
        key: "a".to_owned(),
        modifiers: Modifiers::default(),
        repeat: false,
        text: Some("ä".to_owned()),
        code: None,
        location: 0,
    })
    .unwrap();
    assert_eq!(eval(&page, "document.getElementById('t').value"), "hiä");

    page.insert_text("!".to_owned()).unwrap();
    assert_eq!(eval(&page, "document.getElementById('t').value"), "hiä!");
    browser.close();
}

#[test]
fn a_wheel_tick_scrolls_the_document() {
    let server = spawn_server();
    let browser = browser();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    page.navigate(&server.url("/interact"), WaitUntil::Load)
        .unwrap()
        .unwrap();

    page.dispatch_wheel(WheelInput {
        x: 50.0,
        y: 50.0,
        delta_x: 0.0,
        delta_y: 240.0,
        modifiers: Modifiers::default(),
    })
    .unwrap();
    assert_eq!(eval(&page, "String(window.scrollY)"), "240");

    let metrics = page.layout_metrics().unwrap();
    assert_eq!(metrics.scroll_y, 240.0);
    assert!(metrics.content_height >= 4000.0, "{metrics:?}");
    assert_eq!(metrics.client_height, page.viewport().unwrap().height);
    browser.close();
}

/// The node surface end to end: query → handle → describe → resolve, and the
/// same node named three ways.
#[test]
fn the_node_surface_round_trips_across_the_boundary() {
    let browser = browser();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    page.set_content("<!doctype html><div id=a class=x>text</div><div class=x></div>")
        .unwrap()
        .unwrap();

    let document = page.document_description(0, false).unwrap().unwrap();
    let root = NodeRef::Handle(document.handle);

    let all = page
        .query_selector(root, ".x".to_owned(), true)
        .unwrap()
        .unwrap();
    assert_eq!(all.len(), 2);
    let one = page
        .query_selector(root, "#a".to_owned(), false)
        .unwrap()
        .unwrap();
    assert_eq!(one, vec![all[0]]);

    let described = page
        .describe_node(NodeRef::Handle(one[0]), 1, false)
        .unwrap()
        .unwrap();
    assert_eq!(described.node_name, "DIV");
    assert_eq!(described.children[0].node_value, "text");

    // The same node named as a remote object resolves to the same handle.
    let object = page
        .resolve_node(NodeRef::Handle(one[0]), Some("g".to_owned()))
        .unwrap()
        .unwrap();
    assert_eq!(object.subtype, Some(RemoteSubtype::Node));
    let object_id = object.object_id.unwrap();
    assert_eq!(
        page.node_handle(NodeRef::Object(object_id))
            .unwrap()
            .unwrap(),
        one[0]
    );

    // Geometry, by either name.
    let quads = page.box_quads(NodeRef::Object(object_id)).unwrap().unwrap();
    assert!(quads.width > 0.0 && quads.height > 0.0, "{quads:?}");
    let content = page
        .content_quads(NodeRef::Handle(one[0]))
        .unwrap()
        .unwrap();
    assert_eq!(content.len(), 1);
    assert!(
        page.scroll_into_view_if_needed(NodeRef::Handle(one[0]), None)
            .unwrap()
            .is_ok()
    );

    // A bad selector is data, not a bug; a bogus handle is a real error.
    assert!(
        page.query_selector(root, ":::".to_owned(), false)
            .unwrap()
            .is_err()
    );
    assert!(matches!(
        page.describe_node(NodeRef::Handle(999_999), 0, false)
            .unwrap(),
        Err(RemoteError::NoSuchObject(_))
    ));
    browser.close();
}
