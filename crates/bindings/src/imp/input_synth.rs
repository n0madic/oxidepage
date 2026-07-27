//! Synthesis of trusted input: turning a coordinate or a key into the event
//! *sequences* a browser actually fires.
//!
//! This is the engine side of the UI event family — the `imp` modules for the
//! interfaces themselves stay data-only. Everything here is driven by the
//! embedder (`Page::dispatch_mouse` and friends), never by script, which is why
//! every event it produces has `isTrusted = true`.
//!
//! Two rules govern the whole module:
//!
//! * **Coordinates are viewport CSS pixels.** That is what
//!   `LayoutEngine::elements_from_point` takes and what `clientX`/`clientY`
//!   mean. `pageX/Y` add the document scroll (computed at read time, in
//!   `imp::mouse_event`); `offsetX/Y` subtract the target's padding-box origin.
//! * **Every step re-validates its node ids.** A listener between two events of
//!   one sequence can remove the element under the pointer, navigate, or
//!   rebuild the tree — and a stale `NodeId` panics in `Arena::node`. Nothing
//!   here holds an id across a dispatch without checking it is still connected.

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::events::{
    EventData, EventTargetKey, InputFields, KeyboardFields, Modifiers, MouseFields, UiKind,
    UiPayload, WheelFields,
};
use crate::imp::keys;

/// Which mouse event a [`dispatch_mouse`] call stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseEventKind {
    Move,
    Down,
    Up,
}

/// A synthesized mouse input.
#[derive(Clone, Copy, Debug)]
pub struct MouseInput {
    pub kind: MouseEventKind,
    /// Viewport CSS pixels.
    pub x: f32,
    pub y: f32,
    pub button: i16,
    /// The bitmask of buttons held *during* the event.
    pub buttons: u16,
    pub modifiers: Modifiers,
    /// 1 for a single click, 2 for the second of a double click. Becomes
    /// `UIEvent.detail`.
    pub click_count: i32,
}

/// The element a point resolves to, honouring `pointer-events`.
fn hit_test(cx: &BindCx<'_>, x: f32, y: f32) -> Option<NodeId> {
    let dom = cx.state.dom.borrow();
    let layout = cx.state.layout.borrow();
    layout.elements_from_point(&dom, x, y).first().copied()
}

/// `offsetX`/`offsetY`: the point relative to the target's padding-box origin.
/// `None` when there is no target or no box, which makes the getters fall back
/// to `pageX`/`pageY` exactly as the spec does for a targetless event.
fn offset_in(cx: &BindCx<'_>, target: Option<NodeId>, x: f32, y: f32) -> Option<(f64, f64)> {
    let target = target?;
    let layout = cx.state.layout.borrow();
    let rect = layout.padding_box(target)?;
    // Both sides are already **viewport** coordinates — `padding_box` resolves
    // through `absolute_origin(.., include_scroll: true)`. Adding the document
    // scroll here offset every reading by exactly the scroll position on any
    // page that had been scrolled.
    Some((f64::from(x - rect.origin.x), f64::from(y - rect.origin.y)))
}

/// Builds the payload for one synthesized mouse event.
///
/// Fallible because `relatedTarget` is stored as the node's wrapper (which is
/// what pins it for the life of the event — see `MouseFields::related`), and
/// minting a wrapper can throw.
fn mouse_payload(
    cx: &BindCx<'_>,
    input: &MouseInput,
    target: Option<NodeId>,
    related: Option<NodeId>,
    pointer: bool,
) -> Result<UiPayload, JsThrow> {
    let offset = offset_in(cx, target, input.x, input.y);
    let related = match related {
        Some(id) => Some(cx.node_to_js(id)?),
        None => None,
    };
    let mut payload = UiPayload::new(UiKind::Mouse(Box::new(MouseFields {
        // A headless page has no screen origin, so screen == client.
        screen_x: f64::from(input.x),
        screen_y: f64::from(input.y),
        client_x: f64::from(input.x),
        client_y: f64::from(input.y),
        offset,
        button: input.button,
        buttons: input.buttons,
        related,
        wheel: None,
        pointer: pointer.then(crate::imp::pointer_event::mouse_pointer),
    })));
    payload.detail = input.click_count;
    payload.has_view = true;
    payload.modifiers = input.modifiers;
    Ok(payload)
}

/// Creates a trusted event of `interface` and dispatches it at `target`.
/// Returns `false` when a listener cancelled it.
///
/// `target` is re-validated here rather than by every caller: this is the one
/// point where a node id crosses into a dispatch.
fn fire_at(
    cx: &BindCx<'_>,
    target: NodeId,
    interface: &str,
    event_type: &str,
    bubbles: bool,
    cancelable: bool,
    payload: UiPayload,
) -> Result<bool, JsThrow> {
    if !cx
        .state
        .dom
        .borrow()
        .get(target)
        .is_some_and(|n| n.is_connected())
    {
        return Ok(true);
    }
    let mut data = EventData::new(
        event_type.to_owned(),
        bubbles,
        cancelable,
        /* composed */ true,
    );
    data.is_trusted = true;
    data.time_stamp = cx.now_ms();
    data.ui = Some(Box::new(payload));
    let (value, data) = cx.new_event_object(interface, data)?;
    let proceed = crate::events::dispatch_event(cx, EventTargetKey::Node(target), &value, &data)?;
    crate::microtask_checkpoint(cx);
    Ok(proceed)
}

/// The inclusive-ancestor chain of `node`, innermost first. `None` yields an
/// empty chain, which makes the enter/leave diffs below total.
fn chain(cx: &BindCx<'_>, node: Option<NodeId>) -> Vec<NodeId> {
    let Some(node) = node else {
        return Vec::new();
    };
    let dom = cx.state.dom.borrow();
    if !dom.get(node).is_some_and(|n| n.is_connected()) {
        return Vec::new();
    }
    dom.inclusive_ancestors(node)
        .filter(|&id| {
            dom.get(id)
                .is_some_and(|n| n.data().kind() == oxidepage_dom::NodeKind::Element)
        })
        .collect()
}

/// Fires the `mouseout`/`mouseleave`/`mouseover`/`mouseenter` quartet for a
/// pointer moving from `old` to `new`, then commits the new hover target.
///
/// The order is the whole point, and it is the order the spec fires them in:
/// `mouseout` at the element being left (bubbling), then `mouseleave` on each
/// element of the leave chain **innermost first**, then `mouseover` at the
/// element being entered (bubbling), then `mouseenter` on the enter chain
/// **outermost first**. `mouseenter`/`mouseleave` do not bubble — they are
/// fired individually on every element that is actually being entered or left,
/// which is why they need the chain diff rather than a single dispatch.
fn transfer_hover(
    cx: &BindCx<'_>,
    input: &MouseInput,
    old: Option<NodeId>,
    new: Option<NodeId>,
) -> Result<(), JsThrow> {
    if old == new {
        return Ok(());
    }
    let old_chain = chain(cx, old);
    let new_chain = chain(cx, new);
    // Elements left: in the old chain but not the new one, innermost first.
    let leaving: Vec<NodeId> = old_chain
        .iter()
        .copied()
        .filter(|id| !new_chain.contains(id))
        .collect();
    // Elements entered: in the new chain but not the old one, outermost first.
    let entering: Vec<NodeId> = new_chain
        .iter()
        .copied()
        .filter(|id| !old_chain.contains(id))
        .rev()
        .collect();

    if let Some(old) = old {
        let payload = mouse_payload(cx, input, Some(old), new, false)?;
        fire_at(cx, old, "MouseEvent", "mouseout", true, true, payload)?;
        for &id in &leaving {
            let payload = mouse_payload(cx, input, Some(id), new, false)?;
            fire_at(cx, id, "MouseEvent", "mouseleave", false, false, payload)?;
        }
    }
    if let Some(new) = new {
        let payload = mouse_payload(cx, input, Some(new), old, false)?;
        fire_at(cx, new, "MouseEvent", "mouseover", true, true, payload)?;
        for &id in &entering {
            let payload = mouse_payload(cx, input, Some(id), old, false)?;
            fire_at(cx, id, "MouseEvent", "mouseenter", false, false, payload)?;
        }
    }
    cx.state.dom.borrow_mut().set_hovered(new);
    Ok(())
}

/// The nearest inclusive ancestor of `node` that can take focus, if any.
/// Clicking a `<span>` inside a `<button>` focuses the button.
fn focusable_from(cx: &BindCx<'_>, node: NodeId) -> Option<NodeId> {
    let dom = cx.state.dom.borrow();
    dom.inclusive_ancestors(node)
        .find(|&id| dom.is_focusable(id))
}

/// Runs one synthesized mouse event, with all the state changes and secondary
/// events a browser produces around it.
pub fn dispatch_mouse(cx: &BindCx<'_>, input: MouseInput) -> Result<(), JsThrow> {
    let target = hit_test(cx, input.x, input.y);

    match input.kind {
        MouseEventKind::Move => {
            let old = cx.state.dom.borrow().hovered();
            transfer_hover(cx, &input, old, target)?;
            if let Some(target) = target {
                let payload = mouse_payload(cx, &input, Some(target), None, true)?;
                fire_at(
                    cx,
                    target,
                    "PointerEvent",
                    "pointermove",
                    true,
                    true,
                    payload,
                )?;
                let payload = mouse_payload(cx, &input, Some(target), None, false)?;
                fire_at(cx, target, "MouseEvent", "mousemove", true, true, payload)?;
            }
        }
        MouseEventKind::Down => {
            // The pointer must be over the target before the press: a real
            // sequence always has a move (or the first entry) preceding it, and
            // `:hover` has to be set for `:active` styling to compose.
            let old = cx.state.dom.borrow().hovered();
            transfer_hover(cx, &input, old, target)?;
            let Some(target) = target else {
                return Ok(());
            };
            let payload = mouse_payload(cx, &input, Some(target), None, true)?;
            fire_at(
                cx,
                target,
                "PointerEvent",
                "pointerdown",
                true,
                true,
                payload,
            )?;
            let payload = mouse_payload(cx, &input, Some(target), None, false)?;
            let proceed = fire_at(cx, target, "MouseEvent", "mousedown", true, true, payload)?;

            cx.state.dom.borrow_mut().set_active(Some(target));

            // Focus moves on `mousedown`, and only if it was not cancelled —
            // `preventDefault()` on `mousedown` suppressing focus is the
            // documented way to keep focus in a custom control, and every
            // dropdown/combobox library relies on it.
            if proceed {
                let to = focusable_from(cx, target);
                crate::imp::interaction::set_focus_from_input(cx, to)?;
            }
        }
        MouseEventKind::Up => {
            let pressed = cx.state.dom.borrow().active();
            cx.state.dom.borrow_mut().set_active(None);
            let Some(target) = target else {
                return Ok(());
            };
            let payload = mouse_payload(cx, &input, Some(target), None, true)?;
            fire_at(cx, target, "PointerEvent", "pointerup", true, true, payload)?;
            let payload = mouse_payload(cx, &input, Some(target), None, false)?;
            fire_at(cx, target, "MouseEvent", "mouseup", true, true, payload)?;

            // `click` goes to the nearest common inclusive ancestor of the
            // press and release targets — pressing on one element and releasing
            // on another fires `click` on what contains both, not on either.
            if let Some(click_target) = common_ancestor(cx, pressed, Some(target)) {
                // A `PointerEvent`, per HTML's "fire a synthetic pointer event":
                // being a MouseEvent is what makes `dispatch_event` run the
                // activation behavior, so a synthesized click follows a link and
                // submits a form through exactly the path `.click()` uses.
                let payload = mouse_payload(cx, &input, Some(click_target), None, true)?;
                fire_at(
                    cx,
                    click_target,
                    "PointerEvent",
                    "click",
                    true,
                    true,
                    payload,
                )?;
                if input.button == 2 {
                    let payload = mouse_payload(cx, &input, Some(click_target), None, false)?;
                    fire_at(
                        cx,
                        click_target,
                        "MouseEvent",
                        "contextmenu",
                        true,
                        true,
                        payload,
                    )?;
                }
                if input.click_count == 2 {
                    let payload = mouse_payload(cx, &input, Some(click_target), None, false)?;
                    fire_at(
                        cx,
                        click_target,
                        "MouseEvent",
                        "dblclick",
                        true,
                        true,
                        payload,
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// The nearest common inclusive ancestor of two nodes, or `b` when there is no
/// press target to reconcile with.
fn common_ancestor(cx: &BindCx<'_>, a: Option<NodeId>, b: Option<NodeId>) -> Option<NodeId> {
    let (Some(a), Some(b)) = (a, b) else {
        return b;
    };
    if a == b {
        return Some(a);
    }
    let a_chain = chain(cx, Some(a));
    chain(cx, Some(b))
        .into_iter()
        .find(|id| a_chain.contains(id))
}

// === Keyboard ===

/// Which keyboard event a [`dispatch_key`] call stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyEventKind {
    Down,
    Up,
}

/// A synthesized key press.
#[derive(Clone, Copy, Debug)]
pub struct KeyInput<'a> {
    pub kind: KeyEventKind,
    /// The `KeyboardEvent.key` value: `"a"`, `"A"`, `"Enter"`, `"ArrowLeft"`.
    pub key: &'a str,
    pub modifiers: Modifiers,
    pub repeat: bool,
}

/// The element a key goes to: the focused element, or the body when nothing
/// holds focus — which is what a browser does and what makes a global hotkey
/// listener on `document` work.
fn key_target(cx: &BindCx<'_>) -> Option<NodeId> {
    if let Some(focused) = cx.state.dom.borrow().focused() {
        return Some(focused);
    }
    let document = cx.state.dom.borrow().document();
    crate::imp::document::body(cx, document).ok().flatten()
}

fn keyboard_payload(
    resolved: &keys::ResolvedKey,
    input: &KeyInput<'_>,
    char_code: u32,
) -> UiPayload {
    let mut payload = UiPayload::new(UiKind::Keyboard(Box::new(KeyboardFields {
        key: resolved.key.clone(),
        code: resolved.code.clone(),
        location: 0,
        repeat: input.repeat,
        is_composing: false,
        char_code,
        key_code: resolved.key_code,
    })));
    payload.has_view = true;
    payload.modifiers = input.modifiers;
    payload
}

/// Fires one trusted `beforeinput`/`input` at `target`.
fn fire_input_event(
    cx: &BindCx<'_>,
    target: NodeId,
    event_type: &str,
    cancelable: bool,
    input_type: &str,
    data: Option<String>,
) -> Result<bool, JsThrow> {
    let mut payload = UiPayload::new(UiKind::Input(Box::new(InputFields {
        data,
        is_composing: false,
        input_type: input_type.to_owned(),
    })));
    payload.has_view = true;
    fire_at(
        cx,
        target,
        "InputEvent",
        event_type,
        /* bubbles */ true,
        cancelable,
        payload,
    )
}

/// Runs one synthesized key event and, for a key that edits text, the
/// `beforeinput` → mutate → `input` sequence its default action produces.
pub fn dispatch_key(cx: &BindCx<'_>, input: KeyInput<'_>) -> Result<(), JsThrow> {
    let Some(target) = key_target(cx) else {
        return Ok(());
    };
    let resolved = keys::lookup(input.key);

    if input.kind == KeyEventKind::Up {
        let payload = keyboard_payload(&resolved, &input, 0);
        fire_at(cx, target, "KeyboardEvent", "keyup", true, true, payload)?;
        return Ok(());
    }

    let payload = keyboard_payload(&resolved, &input, 0);
    let proceed = fire_at(cx, target, "KeyboardEvent", "keydown", true, true, payload)?;

    // `keypress` is deprecated and still listened for by jQuery and every
    // hotkey library, so a printable key fires one. It carries `charCode`,
    // which is what `which` reports for it.
    if proceed && let Some(text) = resolved.text.as_deref() {
        let char_code = text.chars().next().map_or(0, |c| c as u32);
        let payload = keyboard_payload(&resolved, &input, char_code);
        fire_at(cx, target, "KeyboardEvent", "keypress", true, true, payload)?;
    }

    // The default action, suppressed when `keydown` was cancelled — that is
    // what `preventDefault()` on `keydown` means and why forms use it.
    if proceed {
        run_key_default_action(cx, target, &resolved, &input)?;
    }
    Ok(())
}

/// The default action of a key: insert text, edit, submit, blur, or move focus.
fn run_key_default_action(
    cx: &BindCx<'_>,
    target: NodeId,
    resolved: &keys::ResolvedKey,
    input: &KeyInput<'_>,
) -> Result<(), JsThrow> {
    // A modifier-held key is a shortcut, not text: `Ctrl+A` must not type "a".
    let shortcut = input.modifiers.ctrl || input.modifiers.meta || input.modifiers.alt;

    match resolved.key.as_str() {
        "Enter" => {
            // Implicit submission: Enter in an *input* text control submits its
            // form. A `<textarea>` is a text entry control too, but there Enter
            // is a newline — HTML scopes implicit submission to the input text
            // states, and submitting a textarea's form instead of inserting the
            // line break loses whatever the user was typing.
            if cx.state.dom.borrow().allows_implicit_submission(target) {
                let form = cx.state.dom.borrow().form_owner(target);
                if let Some(form) = form {
                    crate::imp::form_submit::submit(cx, form, None, /* fire_event */ true)?;
                }
                return Ok(());
            }
            if shortcut {
                return Ok(());
            }
            edit_text(cx, target, Some("\n".to_owned()), false)
        }
        "Escape" => {
            if cx.state.dom.borrow().focused() == Some(target) {
                crate::imp::interaction::set_focus_from_input(cx, None)?;
            }
            Ok(())
        }
        "Tab" => move_sequential_focus(cx, input.modifiers.shift),
        "Backspace" | "Delete" if !shortcut => {
            let forward = resolved.key == "Delete";
            edit_text(cx, target, None, forward)
        }
        _ if !shortcut => match resolved.text.as_deref() {
            Some(text) => edit_text(cx, target, Some(text.to_owned()), false),
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

/// Applies one text edit to a text control: replace the selection with `data`,
/// or — when `data` is `None` — delete either the selection or one character.
///
/// Fires `beforeinput` (cancelable), mutates, then `input`. `change` is **not**
/// fired here: a text control fires it on blur, and only if the value differs
/// from the one it had when focus arrived.
fn edit_text(
    cx: &BindCx<'_>,
    target: NodeId,
    data: Option<String>,
    forward: bool,
) -> Result<(), JsThrow> {
    {
        let dom = cx.state.dom.borrow();
        if !dom.is_text_entry(target) || dom.is_edit_blocked(target) {
            return Ok(());
        }
    }

    let input_type = match (&data, forward) {
        (Some(_), _) => "insertText",
        (None, false) => "deleteContentBackward",
        (None, true) => "deleteContentForward",
    };
    if !fire_input_event(cx, target, "beforeinput", true, input_type, data.clone())? {
        return Ok(());
    }

    // Re-read everything after the dispatch: a `beforeinput` listener may have
    // changed the value or the selection out from under us.
    let applied = {
        let mut dom = cx.state.dom.borrow_mut();
        if !dom.get(target).is_some_and(|n| n.is_connected()) {
            return Ok(());
        }
        let value: Vec<u16> = dom.form_value(target).encode_utf16().collect();
        let (mut start, end, _) = dom.selection(target);
        let mut end = end;

        if data.is_none() && start == end {
            // A collapsed caret deletes one character in the given direction.
            if forward {
                end = (end + 1).min(value.len());
            } else {
                start = start.saturating_sub(1);
            }
        }
        if start == end && data.is_none() {
            return Ok(());
        }

        let inserted: Vec<u16> = data.as_deref().unwrap_or("").encode_utf16().collect();
        let mut next: Vec<u16> = Vec::with_capacity(value.len() - (end - start) + inserted.len());
        next.extend_from_slice(&value[..start]);
        next.extend_from_slice(&inserted);
        next.extend_from_slice(&value[end..]);

        // `maxlength` caps *user* edits only — assigning `value` from script
        // bypasses it, which is why the check lives here and not in the DOM.
        let mut caret = start + inserted.len();
        if let Some(max) = dom.max_length(target)
            && next.len() > max
            && !inserted.is_empty()
        {
            next.truncate(max);
            caret = caret.min(next.len());
        }

        let Ok(text) = String::from_utf16(&next) else {
            return Ok(());
        };
        dom.set_form_value(target, text);
        dom.collapse_selection_to(target, caret);
        true
    };

    if applied {
        fire_input_event(cx, target, "input", false, input_type, data)?;
    }
    Ok(())
}

/// `Input.insertText`: one mutation with one `beforeinput`/`input` pair and no
/// key events at all — a paste, or an IME commit, not typing.
pub fn insert_text(cx: &BindCx<'_>, text: &str) -> Result<(), JsThrow> {
    let Some(target) = key_target(cx) else {
        return Ok(());
    };
    edit_text(cx, target, Some(text.to_owned()), false)
}

/// Moves focus to the next (or previous) element in the sequential focus order.
fn move_sequential_focus(cx: &BindCx<'_>, backward: bool) -> Result<(), JsThrow> {
    let order = {
        let dom = cx.state.dom.borrow();
        dom.sequential_focus_order()
    };
    if order.is_empty() {
        return Ok(());
    }
    let current = cx.state.dom.borrow().focused();
    let index = current.and_then(|id| order.iter().position(|&x| x == id));
    let next = match (index, backward) {
        // Wrapping is what a single browsing context does: there is no chrome
        // to hand focus back to.
        (Some(i), false) => order[(i + 1) % order.len()],
        (Some(i), true) => order[(i + order.len() - 1) % order.len()],
        (None, false) => order[0],
        (None, true) => order[order.len() - 1],
    };
    crate::imp::interaction::set_focus_from_input(cx, Some(next))
}

// === Wheel ===

/// A synthesized wheel tick. Deltas are in CSS pixels (`DOM_DELTA_PIXEL`),
/// positive meaning content moves up/left — the direction a browser reports for
/// scrolling down/right.
#[derive(Clone, Copy, Debug)]
pub struct WheelInput {
    pub x: f32,
    pub y: f32,
    pub delta_x: f64,
    pub delta_y: f64,
    pub modifiers: Modifiers,
}

/// Fires a cancelable `wheel` at the element under the pointer and, unless a
/// listener cancels it, scrolls the nearest scrollable ancestor.
///
/// Cancelling matters: a carousel or a modal that calls `preventDefault()` on
/// `wheel` to trap scrolling must actually trap it, and a driver that ignored
/// the return would scroll the page out from under such a widget.
pub fn dispatch_wheel(cx: &BindCx<'_>, input: WheelInput) -> Result<(), JsThrow> {
    let Some(target) = hit_test(cx, input.x, input.y) else {
        return Ok(());
    };

    let mouse = MouseInput {
        kind: MouseEventKind::Move,
        x: input.x,
        y: input.y,
        button: 0,
        buttons: 0,
        modifiers: input.modifiers,
        click_count: 0,
    };
    let mut payload = mouse_payload(cx, &mouse, Some(target), None, false)?;
    if let UiKind::Mouse(fields) = &mut payload.kind {
        fields.wheel = Some(WheelFields {
            delta_x: input.delta_x,
            delta_y: input.delta_y,
            delta_z: 0.0,
            // `DOM_DELTA_PIXEL`: the deltas are CSS pixels.
            delta_mode: 0,
        });
    }
    // `detail` is 0 for a wheel event, not a click count.
    payload.detail = 0;
    if !fire_at(cx, target, "WheelEvent", "wheel", true, true, payload)? {
        return Ok(());
    }

    scroll_nearest(cx, target, input.delta_x as f32, input.delta_y as f32);
    Ok(())
}

/// Scrolls the nearest scrollable **inclusive** ancestor of `node` by a delta,
/// walking outwards past any container that cannot move in that direction —
/// which is what makes a wheel over an already-bottomed-out inner panel scroll
/// the page.
///
/// "Inclusive" is load-bearing: `scroll_parent` starts at the box's *parent*,
/// so a wheel over an `overflow: auto` container whose children are all text
/// nodes (`elements_from_point` returns the container itself) would otherwise
/// skip the very element the pointer is over and scroll the document.
fn scroll_nearest(cx: &BindCx<'_>, node: NodeId, dx: f32, dy: f32) {
    if cx.state.layout.borrow().is_scroll_container(node) {
        let changed = {
            let mut layout = cx.state.layout.borrow_mut();
            let offset = layout.scroll_offset(node);
            layout
                .set_scroll_offset(node, offset.x + dx, offset.y + dy)
                .changed
        };
        if changed {
            crate::imp::geometry_support::note_scroll(cx, Some(node), true);
            return;
        }
    }
    let mut current = node;
    loop {
        let parent = {
            let dom = cx.state.dom.borrow();
            let layout = cx.state.layout.borrow();
            layout.scroll_parent(&dom, current)
        };
        let (target, changed) = match parent {
            oxidepage_layout::ScrollParent::None => return,
            oxidepage_layout::ScrollParent::Element(container) => {
                let mut layout = cx.state.layout.borrow_mut();
                let offset = layout.scroll_offset(container);
                let result = layout.set_scroll_offset(container, offset.x + dx, offset.y + dy);
                (Some(container), result.changed)
            }
            oxidepage_layout::ScrollParent::DocumentScrollingElement => {
                let mut layout = cx.state.layout.borrow_mut();
                let offset = layout.viewport_scroll();
                let result = layout.set_viewport_scroll(offset.x + dx, offset.y + dy);
                (None, result.changed)
            }
        };
        if changed {
            crate::imp::geometry_support::note_scroll(cx, target, true);
            return;
        }
        // This container could not move; try the next one out.
        match target {
            Some(container) => current = container,
            None => return,
        }
    }
}
