//! Trusted input synthesis: the event *sequences* a coordinate turns into.
//!
//! WPT covers the interfaces and the dispatch algorithm; it cannot reach any of
//! this, because a testharness page has no way to synthesize a real pointer.
//! These tests assert the ordering and the state changes, which is where the
//! bugs live.

use oxidepage_bindings::{KeyEventKind, KeyInput, Modifiers, MouseEventKind, MouseInput};
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

// === Keyboard and text editing ===

/// Types one key: down (which runs the default action) then up.
fn press(page: &Page, key: &str) {
    press_with(page, key, Modifiers::default());
}

fn press_with(page: &Page, key: &str, modifiers: Modifiers) {
    for kind in [KeyEventKind::Down, KeyEventKind::Up] {
        page.dispatch_key(KeyInput {
            kind,
            key,
            modifiers,
            repeat: false,
        });
    }
}

fn type_text(page: &Page, text: &str) {
    for ch in text.chars() {
        press(page, &ch.to_string());
    }
}

/// The event sequence typing produces, and the rule that `change` waits for
/// blur. This is the timing every real form depends on and that end-state
/// assertions hide.
#[test]
fn typing_fires_input_but_change_only_on_blur() {
    let page = page_with(
        r##"<!doctype html>
           <input id=t><button id=other>x</button>
           <script>
             window.log = [];
             const t = document.getElementById("t");
             for (const type of ["keydown", "keypress", "beforeinput", "input", "change", "keyup"]) {
               t.addEventListener(type, e => {
                 log.push(type + (e.inputType ? ":" + e.inputType : ""));
               });
             }
           </script>"##,
    );
    page.eval("document.getElementById('t').focus()").unwrap();
    press(&page, "a");

    assert_eq!(
        eval_string(&page, "window.log.join(',')"),
        "keydown,keypress,beforeinput:insertText,input:insertText,keyup",
        "typing fires beforeinput/input around the mutation, and no change"
    );
    assert_eq!(
        eval_string(&page, "document.getElementById('t').value"),
        "a"
    );

    page.eval("window.log = []; document.getElementById('other').focus()")
        .unwrap();
    assert_eq!(
        eval_string(&page, "window.log.join(',')"),
        "change",
        "change fires on blur, once, because the value differs from focus time"
    );
}

/// Blur without an edit owes no `change` — the comparison is against the value
/// at focus time, not against the default value.
#[test]
fn blur_without_an_edit_fires_no_change() {
    let page = page_with(
        r##"<!doctype html>
           <input id=t value=hello><button id=other>x</button>
           <script>
             window.changes = 0;
             document.getElementById("t").addEventListener("change", () => changes++);
           </script>"##,
    );
    page.eval("document.getElementById('t').focus()").unwrap();
    page.eval("document.getElementById('other').focus()")
        .unwrap();
    assert_eq!(eval_string(&page, "String(window.changes)"), "0");

    // And a round trip back to the same value also owes nothing.
    page.eval("document.getElementById('t').focus()").unwrap();
    type_text(&page, "x");
    press(&page, "Backspace");
    page.eval("document.getElementById('other').focus()")
        .unwrap();
    assert_eq!(
        eval_string(&page, "String(window.changes)"),
        "0",
        "an edit that restores the original value owes no change"
    );
}

/// Text accumulates, `Backspace` deletes, and the caret tracks it.
#[test]
fn typing_edits_the_value() {
    let page = page_with(r##"<!doctype html><input id=t>"##);
    page.eval("document.getElementById('t').focus()").unwrap();
    type_text(&page, "abc");
    assert_eq!(
        eval_string(&page, "document.getElementById('t').value"),
        "abc"
    );

    press(&page, "Backspace");
    assert_eq!(
        eval_string(&page, "document.getElementById('t').value"),
        "ab"
    );
}

/// `maxlength` caps user input.
#[test]
fn maxlength_caps_typing() {
    let page = page_with(r##"<!doctype html><input id=t maxlength=3>"##);
    page.eval("document.getElementById('t').focus()").unwrap();
    type_text(&page, "abcdef");
    assert_eq!(
        eval_string(&page, "document.getElementById('t').value"),
        "abc",
        "maxlength truncates user edits"
    );
}

/// A `readonly` control refuses edits but still fires key events.
#[test]
fn readonly_refuses_edits() {
    let page = page_with(
        r##"<!doctype html><input id=t readonly value=fixed>
           <script>
             window.keys = 0;
             document.getElementById("t").addEventListener("keydown", () => keys++);
           </script>"##,
    );
    page.eval("document.getElementById('t').focus()").unwrap();
    type_text(&page, "abc");
    assert_eq!(
        eval_string(&page, "document.getElementById('t').value"),
        "fixed"
    );
    assert_eq!(
        eval_string(&page, "String(window.keys)"),
        "3",
        "the key events still fire — only the edit is refused"
    );
}

/// `preventDefault()` on `keydown` suppresses the edit but not the events.
#[test]
fn preventdefault_on_keydown_suppresses_the_edit() {
    let page = page_with(
        r##"<!doctype html><input id=t>
           <script>
             document.getElementById("t")
               .addEventListener("keydown", e => e.preventDefault());
           </script>"##,
    );
    page.eval("document.getElementById('t').focus()").unwrap();
    type_text(&page, "abc");
    assert_eq!(eval_string(&page, "document.getElementById('t').value"), "");
}

/// A modifier-held key is a shortcut, not text.
#[test]
fn ctrl_key_does_not_type() {
    let page = page_with(r##"<!doctype html><input id=t>"##);
    page.eval("document.getElementById('t').focus()").unwrap();
    press_with(
        &page,
        "a",
        Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    );
    assert_eq!(
        eval_string(&page, "document.getElementById('t').value"),
        "",
        "Ctrl+A must not insert an 'a'"
    );
}

/// Enter in a text control submits its form.
#[test]
fn enter_submits_the_form() {
    let page = page_with(
        r##"<!doctype html>
           <form id=f onsubmit="window.submitted = true; return false">
             <input id=t>
           </form>"##,
    );
    page.eval("document.getElementById('t').focus()").unwrap();
    press(&page, "Enter");
    assert_eq!(eval_string(&page, "String(window.submitted)"), "true");
}

/// The sequential focus order: positive `tabindex` ascending first, then
/// document order over the natively focusable and `tabindex="0"`.
#[test]
fn tab_follows_the_sequential_focus_order() {
    let page = page_with(
        r##"<!doctype html>
           <input id=a>
           <input id=b tabindex=2>
           <input id=c tabindex=1>
           <input id=d tabindex=-1>
           <input id=e>"##,
    );
    let mut seen = Vec::new();
    for _ in 0..4 {
        press(&page, "Tab");
        seen.push(eval_string(&page, "document.activeElement.id"));
    }
    assert_eq!(
        seen,
        ["c", "b", "a", "e"],
        "positive tabindex first (ascending), then document order; tabindex=-1 is skipped"
    );
}

/// Shift+Tab walks the order backwards.
#[test]
fn shift_tab_walks_backwards() {
    let page = page_with(r##"<!doctype html><input id=a><input id=b><input id=c>"##);
    page.eval("document.getElementById('c').focus()").unwrap();
    press_with(
        &page,
        "Tab",
        Modifiers {
            shift: true,
            ..Modifiers::default()
        },
    );
    assert_eq!(eval_string(&page, "document.activeElement.id"), "b");
}

/// `insert_text` is the paste path: one mutation, one `beforeinput`/`input`
/// pair, and no key events at all.
#[test]
fn insert_text_is_a_single_edit() {
    let page = page_with(
        r##"<!doctype html><input id=t>
           <script>
             window.log = [];
             for (const type of ["keydown", "beforeinput", "input"]) {
               document.getElementById("t")
                 .addEventListener(type, () => log.push(type));
             }
           </script>"##,
    );
    page.eval("document.getElementById('t').focus()").unwrap();
    page.insert_text("pasted");
    assert_eq!(
        eval_string(&page, "document.getElementById('t').value"),
        "pasted"
    );
    assert_eq!(
        eval_string(&page, "window.log.join(',')"),
        "beforeinput,input",
        "insert_text produces no key events"
    );
}

/// The legacy `keyCode`/`which`/`code` members every hotkey library reads.
#[test]
fn keyboard_event_members_are_populated() {
    let page = page_with(
        r##"<!doctype html><input id=t>
           <script>
             window.seen = "";
             document.getElementById("t").addEventListener("keydown", e => {
               seen = [e.key, e.code, e.keyCode, e.which, e.shiftKey].join(",");
             });
           </script>"##,
    );
    page.eval("document.getElementById('t').focus()").unwrap();
    press_with(
        &page,
        "A",
        Modifiers {
            shift: true,
            ..Modifiers::default()
        },
    );
    assert_eq!(
        eval_string(&page, "window.seen"),
        "A,KeyA,65,65,true",
        "an uppercase A is the physical KeyA with shift held"
    );
}

/// The selection API: offsets, `setSelectionRange`, `select()`, and the `null`
/// a control without text entry reports.
#[test]
fn selection_api() {
    let page = page_with(
        r##"<!doctype html>
           <input id=t value="hello world">
           <input id=c type=checkbox>
           <textarea id=a>abc</textarea>"##,
    );

    // Focus places a collapsed caret at the end.
    page.eval("document.getElementById('t').focus()").unwrap();
    assert_eq!(
        eval_string(
            &page,
            "[t.selectionStart, t.selectionEnd, t.selectionDirection].join(',')"
        ),
        "11,11,none"
    );

    page.eval("t.setSelectionRange(0, 5, 'backward')").unwrap();
    assert_eq!(
        eval_string(
            &page,
            "[t.selectionStart, t.selectionEnd, t.selectionDirection].join(',')"
        ),
        "0,5,backward"
    );

    // Typing replaces the selection.
    type_text(&page, "X");
    assert_eq!(eval_string(&page, "t.value"), "X world");

    page.eval("t.select()").unwrap();
    assert_eq!(
        eval_string(&page, "[t.selectionStart, t.selectionEnd].join(',')"),
        "0,7",
        "select() covers the whole value"
    );

    // A control with no text entry reports null, not 0 — 0 is a valid caret.
    assert_eq!(
        eval_string(&page, "String(document.getElementById('c').selectionStart)"),
        "null"
    );
    // A textarea has text entry like an input.
    assert_eq!(
        eval_string(&page, "String(document.getElementById('a').selectionStart)"),
        "3"
    );
}

/// Backspace over a selection deletes the selection, not one character.
#[test]
fn backspace_deletes_the_selection() {
    let page = page_with(r##"<!doctype html><input id=t value="abcdef">"##);
    page.eval("document.getElementById('t').focus(); t.setSelectionRange(1, 4)")
        .unwrap();
    press(&page, "Backspace");
    assert_eq!(eval_string(&page, "t.value"), "aef");
    assert_eq!(eval_string(&page, "String(t.selectionStart)"), "1");
}

/// `maxLength`/`minLength` reflect, with -1 for absent, and reject negatives.
#[test]
fn max_length_reflects() {
    let page = page_with(r##"<!doctype html><input id=t maxlength=5>"##);
    assert_eq!(eval_string(&page, "String(t.maxLength)"), "5");
    assert_eq!(eval_string(&page, "String(t.minLength)"), "-1");

    page.eval("t.maxLength = 9").unwrap();
    assert_eq!(eval_string(&page, "t.getAttribute('maxlength')"), "9");
    assert_eq!(
        eval_string(
            &page,
            "(() => { try { t.maxLength = -1; return 'no throw' } catch (e) { return e.name } })()"
        ),
        "IndexSizeError"
    );
}
