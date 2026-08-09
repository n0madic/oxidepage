//! Input across a frame boundary (ADR-0035 D8).
//!
//! Hit testing crosses; events do not. A click over an `<iframe>` lands on
//! whatever the frame is showing, but the event it produces propagates inside
//! that frame only — `composed` does not change that, and the embedder's
//! listeners never see it.

use std::time::Duration;

use oxidepage_bindings::{Modifiers, MouseEventKind, MouseInput};
use oxidepage_page::{Page, PageOptions, load_html_page};

/// A press-and-release at one point — move, down, up — the way a driver spells
/// a click.
fn click_at(page: &Page, x: f32, y: f32) {
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
            buttons: if kind == MouseEventKind::Down { 1 } else { 0 },
            modifiers: Modifiers::default(),
            click_count: 1,
        });
    }
}

fn page(html: &str) -> oxidepage_page::Page {
    let page = load_html_page(html, PageOptions::default()).unwrap();
    page.settle(Duration::from_millis(500));
    page
}

fn s(page: &oxidepage_page::Page, expr: &str) -> String {
    page.eval_to_string(expr).unwrap()
}

/// The viewport-space centre of an element, read from the page rather than
/// assumed. An `<iframe>` is an *inline-block* replaced box, so it shares a
/// line box with its siblings and baseline-aligns inside it — coordinates
/// worked out by hand are wrong more often than not.
fn centre_of(page: &oxidepage_page::Page, selector: &str) -> (f32, f32) {
    let raw = s(
        page,
        &format!(
            "(() => {{ const r = document.querySelector('{selector}').getBoundingClientRect(); \
              return (r.left + r.width / 2) + ',' + (r.top + r.height / 2); }})()"
        ),
    );
    let (x, y) = raw.split_once(',').expect("a rect centre");
    (x.parse().expect("x"), y.parse().expect("y"))
}

/// A click over an `<iframe>` reaches the element the frame is showing there —
/// not the `<iframe>` itself.
#[test]
fn a_click_lands_inside_the_frame() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <iframe id='f' width='200' height='100' style='border:0' \
          srcdoc='<body style=\"margin:0\">\
            <button id=b style=\"width:120px;height:40px\">go</button>\
            <script>document.getElementById(\"b\").addEventListener(\"click\", () => {\
              document.title = \"clicked\"; });</script></body>'>\
         </iframe></body>",
    );
    page.settle(Duration::from_millis(500));

    // The frame's own origin plus the button's centre inside it.
    let (fx, fy) = centre_of(&page, "#f");
    click_at(&page, fx - 40.0, fy - 30.0);
    page.settle(Duration::from_millis(500));

    assert_eq!(
        s(&page, "document.getElementById('f').contentDocument.title"),
        "clicked",
        "the click crossed into the frame"
    );
}

/// The frame's position offsets the crossing: a click at the page coordinates
/// of the frame's own origin, not at the page origin.
#[test]
fn the_crossing_accounts_for_the_frames_position() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <div style='height:50px'></div>\
         <iframe id='f' width='200' height='100' style='border:0;margin-left:30px' \
          srcdoc='<body style=\"margin:0\">\
            <button id=b style=\"width:100px;height:30px\">go</button>\
            <script>document.getElementById(\"b\").addEventListener(\"click\", () => {\
              document.title = \"hit\"; });</script></body>'>\
         </iframe></body>",
    );
    page.settle(Duration::from_millis(500));

    // Above the frame: nothing inside it is pressed.
    let (fx, fy) = centre_of(&page, "#f");
    click_at(&page, fx, fy - 90.0);
    page.settle(Duration::from_millis(300));
    assert_eq!(
        s(&page, "document.getElementById('f').contentDocument.title"),
        "",
        "a click above the frame does not reach into it"
    );

    // Inside the button, which sits at the frame's own top-left.
    click_at(&page, fx - 60.0, fy - 35.0);
    page.settle(Duration::from_millis(500));
    assert_eq!(
        s(&page, "document.getElementById('f').contentDocument.title"),
        "hit"
    );
}

/// Events do not cross a document boundary, as the spec says: a listener on the
/// embedder never sees a click that landed inside the frame.
#[test]
fn an_event_inside_a_frame_does_not_reach_the_embedder() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <iframe id='f' width='200' height='100' style='border:0' \
          srcdoc='<body style=\"margin:0\"><button id=b style=\"width:120px;height:40px\">go</button></body>'>\
         </iframe>\
         <script>window.outer = 0;\
           document.addEventListener('click', () => { window.outer += 1; }, true);\
         </script></body>",
    );
    page.settle(Duration::from_millis(500));

    let (fx, fy) = centre_of(&page, "#f");
    click_at(&page, fx - 40.0, fy - 30.0);
    page.settle(Duration::from_millis(500));

    assert_eq!(
        s(&page, "window.outer"),
        "0",
        "the embedder's capturing listener must not see the frame's click"
    );
}

/// A click outside every frame still works — the descent must not swallow
/// ordinary hits.
#[test]
fn a_click_outside_a_frame_is_unaffected() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <button id='b' style='width:100px;height:30px'>go</button>\
         <iframe id='f' width='200' height='100'></iframe>\
         <script>window.hits = 0;\
           document.getElementById('b').addEventListener('click', () => { window.hits += 1; });\
         </script></body>",
    );
    let (bx, by) = centre_of(&page, "#b");
    click_at(&page, bx, by);
    page.settle(Duration::from_millis(300));
    assert_eq!(s(&page, "window.hits"), "1");
}
