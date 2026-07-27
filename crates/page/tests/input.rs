//! Trusted input synthesis: the event *sequences* a coordinate turns into.
//!
//! WPT covers the interfaces and the dispatch algorithm; it cannot reach any of
//! this, because a testharness page has no way to synthesize a real pointer.
//! These tests assert the ordering and the state changes, which is where the
//! bugs live.

use oxidepage_bindings::{
    KeyEventKind, KeyInput, Modifiers, MouseEventKind, MouseInput, WheelInput,
};
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

/// `offsetX/Y` are measured against the target's padding box **in viewport
/// coordinates**, so scrolling the document must not shift them: both sides of
/// the subtraction move together. Adding the document scroll to one of them
/// offset every reading by exactly the scroll position.
#[test]
fn offset_coordinates_ignore_the_document_scroll() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #pad { height: 2000px }
             #b { position: absolute; left: 0; top: 500px; width: 100px; height: 100px }
           </style>
           <div id=pad></div><div id=b></div>
           <script>
             window.seen = "";
             document.getElementById("b").addEventListener("click", e => {
               seen = [e.offsetX, e.offsetY, e.pageY].join(",");
             });
           </script>"##,
    );
    page.eval("window.scrollTo(0, 300)").unwrap();
    // The box's top is now at viewport y = 200; a click 10px into it.
    click_at(&page, 10.0, 210.0);
    assert_eq!(
        eval_string(&page, "window.seen"),
        // offset is padding-box relative (10, 10); pageY adds the scroll.
        "10,10,510",
        "offsetX/Y are padding-box relative regardless of scroll, pageY is not"
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

/// Enter in a `<textarea>` is a **newline**, not an implicit submission. HTML
/// scopes implicit submission to the input text states; treating every text
/// entry control as one submitted the form and lost the line break.
#[test]
fn enter_in_a_textarea_inserts_a_newline() {
    let page = page_with(
        r##"<!doctype html>
           <form id=f onsubmit="window.submitted = true; return false">
             <textarea id=a>ab</textarea>
           </form>"##,
    );
    page.eval("document.getElementById('a').focus(); a.setSelectionRange(1, 1)")
        .unwrap();
    press(&page, "Enter");
    assert_eq!(
        eval_string(&page, "JSON.stringify(a.value)"),
        "\"a\\nb\"",
        "Enter inserts a line break at the caret"
    );
    assert_eq!(
        eval_string(&page, "String(window.submitted)"),
        "undefined",
        "and must not submit the owning form"
    );
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

/// The two selection setters move *different* endpoints when the new value
/// would invert the selection: `selectionStart` drags the end up, but
/// `selectionEnd` drags the **start** down. Clamping the end up to the old
/// start instead left the caret where it was.
#[test]
fn selection_end_below_the_start_drags_the_start_down() {
    let page = page_with(r##"<!doctype html><input id=t value="abcdef">"##);
    page.eval("t.setSelectionRange(5, 5); t.selectionEnd = 2")
        .unwrap();
    assert_eq!(
        eval_string(&page, "[t.selectionStart, t.selectionEnd].join(',')"),
        "2,2",
        "an end below the start moves the start to it"
    );

    // The mirror case still clamps the other way.
    page.eval("t.setSelectionRange(1, 2); t.selectionStart = 4")
        .unwrap();
    assert_eq!(
        eval_string(&page, "[t.selectionStart, t.selectionEnd].join(',')"),
        "4,4",
        "a start past the end drags the end along"
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

// === Wheel and scrollIntoView ===

fn wheel(page: &Page, x: f32, y: f32, dx: f64, dy: f64) {
    page.dispatch_wheel(WheelInput {
        x,
        y,
        delta_x: dx,
        delta_y: dy,
        modifiers: Modifiers::default(),
    });
}

/// A wheel tick fires a cancelable `wheel` and scrolls the nearest scrollable
/// ancestor.
#[test]
fn wheel_scrolls_the_nearest_container() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #box { position: absolute; left: 0; top: 0; width: 200px; height: 200px;
                    overflow: scroll }
             #tall { width: 100px; height: 2000px }
           </style>
           <div id=box><div id=tall></div></div>
           <script>
             window.deltas = "";
             document.getElementById("box").addEventListener("wheel", e => {
               deltas = [e.deltaX, e.deltaY, e.deltaMode].join(",");
             });
           </script>"##,
    );
    wheel(&page, 50.0, 50.0, 0.0, 120.0);
    assert_eq!(
        eval_string(&page, "window.deltas"),
        "0,120,0",
        "the wheel event carries its deltas in pixel mode"
    );
    assert_eq!(
        eval_string(&page, "String(document.getElementById('box').scrollTop)"),
        "120",
        "the nearest scrollable ancestor scrolls"
    );
}

/// The hit element may *be* the scroller. `scrollParent()` answers a strictly
/// ancestor question, so routing the wheel through it alone skipped the
/// container whenever the point did not land on an element *child* of it —
/// and scrolled the document instead. Here the point is to the right of the
/// only child, so `elements_from_point` returns the container itself.
#[test]
fn wheel_scrolls_the_container_it_is_over() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #pad { height: 3000px }
             #box { position: absolute; left: 0; top: 0; width: 200px; height: 100px;
                    overflow: scroll }
             #tall { width: 100px; height: 2000px }
           </style>
           <div id=box><div id=tall></div></div>
           <div id=pad></div>"##,
    );
    wheel(&page, 150.0, 50.0, 0.0, 60.0);
    assert_eq!(
        eval_string(&page, "String(document.getElementById('box').scrollTop)"),
        "60",
        "the container under the pointer scrolls, not the document"
    );
    assert_eq!(
        eval_string(&page, "String(window.scrollY)"),
        "0",
        "the document must not have scrolled instead"
    );
}

/// `preventDefault()` on `wheel` traps the scroll — what every carousel and
/// modal relies on.
#[test]
fn wheel_can_be_canceled() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #box { position: absolute; left: 0; top: 0; width: 200px; height: 200px;
                    overflow: scroll }
             #tall { width: 100px; height: 2000px }
           </style>
           <div id=box><div id=tall></div></div>
           <script>
             document.getElementById("box")
               .addEventListener("wheel", e => e.preventDefault());
           </script>"##,
    );
    wheel(&page, 50.0, 50.0, 0.0, 120.0);
    assert_eq!(
        eval_string(&page, "String(document.getElementById('box').scrollTop)"),
        "0",
        "a canceled wheel must not scroll"
    );
}

/// `scrollIntoView` brings an element into the viewport, and a second call on
/// something already visible is a no-op (the `nearest` default).
#[test]
fn scroll_into_view_reveals_and_is_idempotent() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #spacer { height: 3000px }
             #target { height: 50px }
           </style>
           <div id=spacer></div><div id=target></div>"##,
    );
    assert_eq!(eval_string(&page, "String(window.scrollY)"), "0");

    page.eval("document.getElementById('target').scrollIntoView()")
        .unwrap();
    // `block: start` asks for the element's top at the viewport top; the
    // document cannot scroll past its own end, so the assertion is that the
    // element is *visible*, which is what the method promises.
    assert_eq!(
        eval_string(
            &page,
            "(() => { const r = document.getElementById('target').getBoundingClientRect();
                      return r.top >= 0 && r.bottom <= innerHeight })()"
        ),
        "true",
        "the target is inside the viewport after scrollIntoView"
    );
    assert!(
        eval_string(&page, "String(window.scrollY)") != "0",
        "the document actually scrolled"
    );

    // Already visible, and `nearest` scrolls the minimum — so nothing moves.
    let before = eval_string(&page, "String(window.scrollY)");
    page.eval("document.getElementById('target').scrollIntoView({block: 'nearest'})")
        .unwrap();
    assert_eq!(
        eval_string(&page, "String(window.scrollY)"),
        before,
        "scrollIntoView on an already-visible element is a no-op under `nearest`"
    );
}

/// The element ends up visible in *both* a nested scroll container and the
/// viewport — scrolling only the innermost one leaves it off-screen.
#[test]
fn scroll_into_view_walks_every_ancestor() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #pad { height: 2000px }
             #box { height: 200px; overflow: scroll }
             #inner { height: 3000px }
             #target { position: relative; top: 2500px; height: 50px }
           </style>
           <div id=pad></div>
           <div id=box><div id=inner><div id=target></div></div></div>"##,
    );
    page.eval("document.getElementById('target').scrollIntoView()")
        .unwrap();
    assert!(
        eval_string(&page, "String(document.getElementById('box').scrollTop)") != "0",
        "the inner container scrolled"
    );
    assert!(
        eval_string(&page, "String(window.scrollY)") != "0",
        "and so did the document"
    );
}

/// Inside a scroll container, `scrollIntoView` is idempotent for the same
/// reason it is against the viewport: the box position handed to the alignment
/// is the *visual* delta from the container's near edge, and the container's
/// current offset is added once, by the alignment. Adding it to the delta too
/// scrolled by the whole distance again on every call.
#[test]
fn scroll_into_view_in_a_container_is_idempotent() {
    let page = page_with(
        r##"<!doctype html><style>
             body { margin: 0 }
             #box { height: 200px; overflow: scroll }
             #inner { height: 3000px }
             #target { position: relative; top: 1000px; height: 50px }
           </style>
           <div id=box><div id=inner><div id=target></div></div></div>"##,
    );
    page.eval("document.getElementById('target').scrollIntoView()")
        .unwrap();
    let first = eval_string(&page, "String(document.getElementById('box').scrollTop)");
    assert_eq!(
        first, "1000",
        "the target's top is brought to the near edge"
    );

    page.eval("document.getElementById('target').scrollIntoView()")
        .unwrap();
    assert_eq!(
        eval_string(&page, "String(document.getElementById('box').scrollTop)"),
        first,
        "a second call on an element already at the start must not move anything"
    );

    // `nearest` sees the element as visible now, and must likewise not move.
    page.eval("document.getElementById('target').scrollIntoView({block: 'nearest'})")
        .unwrap();
    assert_eq!(
        eval_string(&page, "String(document.getElementById('box').scrollTop)"),
        first,
        "`nearest` on a visible element is a no-op inside a container too"
    );

    // And `nearest` from *below* the element scrolls back up to it, which the
    // double-counted position could never report (its delta was never negative).
    page.eval(
        "document.getElementById('box').scrollTop = 2000;\
               document.getElementById('target').scrollIntoView({block: 'nearest'})",
    )
    .unwrap();
    assert_eq!(
        eval_string(&page, "String(document.getElementById('box').scrollTop)"),
        "1000",
        "an element above the visible top is scrolled back into view"
    );
}

/// `document.hasFocus()` is true for the rendered document and false for one
/// with no browsing context.
#[test]
fn has_focus_reflects_the_browsing_context() {
    let page = page_with(r##"<!doctype html><p>x</p>"##);
    assert_eq!(eval_string(&page, "String(document.hasFocus())"), "true");
    assert_eq!(
        eval_string(
            &page,
            "String(new DOMParser().parseFromString('<p>', 'text/html').hasFocus())"
        ),
        "false",
        "a document with no browsing context does not have focus"
    );
}
