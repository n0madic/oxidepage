//! Trusted input synthesis: the event *sequences* a coordinate turns into.
//!
//! WPT covers the interfaces and the dispatch algorithm; it cannot reach any of
//! this, because a testharness page has no way to synthesize a real pointer.
//! These tests assert the ordering and the state changes, which is where the
//! bugs live.

use oxidepage_bindings::{Modifiers, MouseEventKind, MouseInput};
use oxidepage_page::{Page, PageOptions, load_html_page};

/// A press-and-release at one point, the way an automation driver spells a
/// click: move, down, up.
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

fn move_to(page: &Page, x: f32, y: f32) {
    page.dispatch_mouse(MouseInput {
        kind: MouseEventKind::Move,
        x,
        y,
        button: 0,
        buttons: 0,
        modifiers: Modifiers::default(),
        click_count: 1,
    });
}

fn eval_string(page: &Page, expr: &str) -> String {
    page.eval_to_string(expr).expect("eval failed")
}

/// A page laid out in a known viewport, so the coordinates in these tests mean
/// what they say.
fn page_with(html: &str) -> Page {
    load_html_page(html, PageOptions::default()).expect("page")
}

/// The full quartet, in order, across a nested pair of boxes. `mouseenter`
/// fires outermost-first and `mouseleave` innermost-first, and neither bubbles
/// — this is the ordering that hover menus depend on.
#[test]
fn hover_chain_ordering() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #outer { position: absolute; left: 0; top: 0; width: 200px; height: 200px }
             #inner { position: absolute; left: 50px; top: 50px; width: 50px; height: 50px }
             #away  { position: absolute; left: 300px; top: 0; width: 50px; height: 50px }
           </style>
           <div id=outer><div id=inner></div></div><div id=away></div>
           <script>
             window.log = [];
             for (const id of ["outer", "inner", "away"]) {
               for (const type of ["mouseover", "mouseout", "mouseenter", "mouseleave"]) {
                 document.getElementById(id).addEventListener(type, e => {
                   log.push(id + ":" + type);
                 });
               }
             }
           </script>"##,
    );

    // Into the inner box. `mouseover` is dispatched once, at the deepest hit
    // element, and bubbles — so `outer` sees it as a bubbled event, not as a
    // second dispatch. `mouseenter` does not bubble and is fired individually
    // on each newly-entered element, outermost first.
    move_to(&page, 60.0, 60.0);
    assert_eq!(
        eval_string(&page, "window.log.join(',')"),
        "inner:mouseover,outer:mouseover,outer:mouseenter,inner:mouseenter",
        "entering a nested box enters the ancestors outermost-first"
    );

    // Out to a sibling: leave inner then outer, enter away.
    page.eval("window.log = []").unwrap();
    move_to(&page, 310.0, 10.0);
    assert_eq!(
        eval_string(&page, "window.log.join(',')"),
        "inner:mouseout,outer:mouseout,inner:mouseleave,outer:mouseleave,\
away:mouseover,away:mouseenter",
        "leaving fires innermost-first and does not re-enter the common ancestor"
    );
}

/// `:hover` must actually restyle, not merely fire events.
#[test]
fn hover_restyles() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #b { position: absolute; left: 0; top: 0; width: 100px; height: 100px;
                  color: rgb(0, 0, 0) }
             #b:hover { color: rgb(0, 128, 0) }
           </style><div id=b></div>"##,
    );
    let color = "getComputedStyle(document.getElementById('b')).color";
    assert_eq!(eval_string(&page, color), "rgb(0, 0, 0)");

    move_to(&page, 50.0, 50.0);
    assert_eq!(
        eval_string(&page, color),
        "rgb(0, 128, 0)",
        ":hover must restyle the element under the pointer"
    );

    move_to(&page, 500.0, 500.0);
    assert_eq!(
        eval_string(&page, color),
        "rgb(0, 0, 0)",
        "moving away must drop :hover"
    );
}

/// `:hover` applies to ancestors too, which is what makes a hover rule on a
/// menu container work.
#[test]
fn hover_applies_to_ancestors() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #outer { position: absolute; left: 0; top: 0; width: 200px; height: 200px;
                      color: rgb(0, 0, 0) }
             #outer:hover { color: rgb(0, 0, 255) }
             #inner { position: absolute; left: 10px; top: 10px; width: 50px; height: 50px }
           </style><div id=outer><div id=inner></div></div>"##,
    );
    move_to(&page, 20.0, 20.0);
    assert_eq!(
        eval_string(
            &page,
            "getComputedStyle(document.getElementById('outer')).color"
        ),
        "rgb(0, 0, 255)",
        "hovering a descendant puts the ancestor in :hover"
    );
}

/// A press focuses the nearest focusable ancestor, and `preventDefault()` on
/// `mousedown` suppresses that — the documented way custom dropdowns keep focus.
#[test]
fn mousedown_moves_focus_unless_canceled() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             button { position: absolute; left: 0; top: 0; width: 100px; height: 50px }
             #other { position: absolute; left: 0; top: 100px; width: 100px; height: 50px }
           </style>
           <button id=b><span id=s>x</span></button>
           <button id=other>y</button>"##,
    );

    // Clicking the span inside the button focuses the button, not the span.
    click_at(&page, 50.0, 25.0);
    assert_eq!(
        eval_string(&page, "document.activeElement.id"),
        "b",
        "focus goes to the nearest focusable ancestor of the hit element"
    );

    page.eval(
        "document.getElementById('other').addEventListener('mousedown', e => e.preventDefault())",
    )
    .unwrap();
    click_at(&page, 50.0, 125.0);
    assert_eq!(
        eval_string(&page, "document.activeElement.id"),
        "b",
        "preventDefault() on mousedown must suppress the focus transfer"
    );
}

/// The synthesized click runs activation behavior: this is what makes a
/// coordinate able to follow a link, and it must go through the same path
/// `.click()` uses.
#[test]
fn click_activates_a_link() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             a { position: absolute; left: 0; top: 0; width: 100px; height: 50px }
           </style><a id=a href="#target">go</a>"##,
    );
    click_at(&page, 50.0, 25.0);
    assert!(
        eval_string(&page, "location.hash") == "#target",
        "a synthesized click must follow the hyperlink"
    );
}

/// `pointer-events: none` makes the overlay transparent to the hit test, so the
/// click reaches what is behind it. Without this, a page with a full-viewport
/// scrim is entirely undriveable.
#[test]
fn pointer_events_none_falls_through() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #under { position: absolute; left: 0; top: 0; width: 200px; height: 200px }
             #over  { position: absolute; left: 0; top: 0; width: 200px; height: 200px;
                      pointer-events: none }
           </style>
           <div id=under></div><div id=over></div>
           <script>
             window.hit = "";
             document.getElementById("under").addEventListener("click", () => hit = "under");
             document.getElementById("over").addEventListener("click", () => hit = "over");
           </script>"##,
    );
    click_at(&page, 50.0, 50.0);
    assert_eq!(
        eval_string(&page, "window.hit"),
        "under",
        "the click must fall through the pointer-events:none overlay"
    );
}

/// Pointer events precede their mouse counterparts, and the click carries a
/// `PointerEvent` so that activation runs.
#[test]
fn pointer_events_precede_mouse_events() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #b { position: absolute; left: 0; top: 0; width: 100px; height: 100px }
           </style><div id=b></div>
           <script>
             window.log = [];
             for (const t of ["pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {
               document.getElementById("b").addEventListener(t, e => {
                 log.push(t + "/" + (e instanceof PointerEvent ? "pointer" : "mouse"));
               });
             }
           </script>"##,
    );
    click_at(&page, 50.0, 50.0);
    assert_eq!(
        eval_string(&page, "window.log.join(',')"),
        "pointerdown/pointer,mousedown/mouse,pointerup/pointer,mouseup/mouse,click/pointer",
        "each pointer event precedes its mouse counterpart"
    );
}

/// Coordinates: `clientX/Y` are what was asked for, `offsetX/Y` are relative to
/// the target's padding box.
#[test]
fn coordinates_are_reported_correctly() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #b { position: absolute; left: 30px; top: 20px; width: 100px; height: 100px;
                  border: 5px solid black; padding: 10px }
           </style><div id=b></div>
           <script>
             window.seen = null;
             document.getElementById("b").addEventListener("click", e => {
               seen = [e.clientX, e.clientY, e.offsetX, e.offsetY, e.button, e.detail].join(",");
             });
           </script>"##,
    );
    click_at(&page, 60.0, 50.0);
    assert_eq!(
        eval_string(&page, "window.seen"),
        // padding box origin = (30 + 5, 20 + 5); offset = (60-35, 50-25)
        "60,50,25,25,0,1",
        "clientX/Y are viewport pixels and offsetX/Y are padding-box relative"
    );
}

/// A listener that removes the element mid-sequence must not take the engine
/// down: every step re-validates its node ids.
#[test]
fn listener_removing_the_target_is_survivable() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #b { position: absolute; left: 0; top: 0; width: 100px; height: 100px }
           </style><div id=b></div>
           <script>
             window.fired = [];
             const b = document.getElementById("b");
             b.addEventListener("mousedown", () => { fired.push("down"); b.remove(); });
             b.addEventListener("mouseup", () => fired.push("up"));
             b.addEventListener("click", () => fired.push("click"));
           </script>"##,
    );
    click_at(&page, 50.0, 50.0);
    assert_eq!(
        eval_string(&page, "window.fired.join(',')"),
        "down",
        "removing the target during mousedown stops the rest of the sequence cleanly"
    );
    assert_eq!(
        eval_string(&page, "String(document.getElementById('b'))"),
        "null"
    );
}

/// HTML's "navigate to a `javascript:` URL": the payload runs, and only a
/// *string* result replaces the document. Every real `href="javascript:..."`
/// handler returns undefined and must leave the page untouched.
#[test]
fn javascript_url_runs_without_navigating() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             a { position: absolute; left: 0; top: 0; width: 100px; height: 50px }
           </style>
           <a id=a href="javascript:window.ran = (window.ran || 0) + 1">go</a>
           <div id=keep>original</div>"##,
    );
    click_at(&page, 50.0, 25.0);
    assert_eq!(
        eval_string(&page, "String(window.ran)"),
        "1",
        "the javascript: payload runs on activation"
    );
    assert_eq!(
        eval_string(&page, "document.getElementById('keep').textContent"),
        "original",
        "an undefined result must not replace the document"
    );
}

/// The other half: a string result *does* replace the document.
#[test]
fn javascript_url_returning_a_string_replaces_the_document() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             a { position: absolute; left: 0; top: 0; width: 100px; height: 50px }
           </style>
           <a id=a href="javascript:'<p id=fresh>replaced</p>'">go</a>"##,
    );
    click_at(&page, 50.0, 25.0);
    assert_eq!(
        eval_string(&page, "document.getElementById('fresh').textContent"),
        "replaced",
        "a string result replaces the document with it"
    );
}
