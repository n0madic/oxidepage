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
use crate::events::{EventData, EventTargetKey, Modifiers, MouseFields, UiKind, UiPayload};

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
/// Falls back to the client coordinates for a target with no box (a hit on a
/// display-less element cannot happen, but a listener may have detached it).
fn offset_in(cx: &BindCx<'_>, target: Option<NodeId>, x: f32, y: f32) -> (f64, f64) {
    let Some(target) = target else {
        return (f64::from(x), f64::from(y));
    };
    let layout = cx.state.layout.borrow();
    let scroll = layout.viewport_scroll();
    let Some(rect) = layout.padding_box(target) else {
        return (f64::from(x), f64::from(y));
    };
    // `padding_box` is in document coordinates; the input is in viewport ones.
    (
        f64::from(x + scroll.x - rect.origin.x),
        f64::from(y + scroll.y - rect.origin.y),
    )
}

/// Builds the payload for one synthesized mouse event.
fn mouse_payload(
    cx: &BindCx<'_>,
    input: &MouseInput,
    target: Option<NodeId>,
    related: Option<NodeId>,
    pointer: bool,
) -> UiPayload {
    let (offset_x, offset_y) = offset_in(cx, target, input.x, input.y);
    let mut payload = UiPayload::new(UiKind::Mouse(Box::new(MouseFields {
        // A headless page has no screen origin, so screen == client.
        screen_x: f64::from(input.x),
        screen_y: f64::from(input.y),
        client_x: f64::from(input.x),
        client_y: f64::from(input.y),
        offset_x,
        offset_y,
        button: input.button,
        buttons: input.buttons,
        related,
        wheel: None,
        pointer: pointer.then(crate::imp::pointer_event::mouse_pointer),
    })));
    payload.detail = input.click_count;
    payload.has_view = true;
    payload.modifiers = input.modifiers;
    payload
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
        let payload = mouse_payload(cx, input, Some(old), new, false);
        fire_at(cx, old, "MouseEvent", "mouseout", true, true, payload)?;
        for &id in &leaving {
            let payload = mouse_payload(cx, input, Some(id), new, false);
            fire_at(cx, id, "MouseEvent", "mouseleave", false, false, payload)?;
        }
    }
    if let Some(new) = new {
        let payload = mouse_payload(cx, input, Some(new), old, false);
        fire_at(cx, new, "MouseEvent", "mouseover", true, true, payload)?;
        for &id in &entering {
            let payload = mouse_payload(cx, input, Some(id), old, false);
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
                let payload = mouse_payload(cx, &input, Some(target), None, true);
                fire_at(
                    cx,
                    target,
                    "PointerEvent",
                    "pointermove",
                    true,
                    true,
                    payload,
                )?;
                let payload = mouse_payload(cx, &input, Some(target), None, false);
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
            let payload = mouse_payload(cx, &input, Some(target), None, true);
            fire_at(
                cx,
                target,
                "PointerEvent",
                "pointerdown",
                true,
                true,
                payload,
            )?;
            let payload = mouse_payload(cx, &input, Some(target), None, false);
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
            let payload = mouse_payload(cx, &input, Some(target), None, true);
            fire_at(cx, target, "PointerEvent", "pointerup", true, true, payload)?;
            let payload = mouse_payload(cx, &input, Some(target), None, false);
            fire_at(cx, target, "MouseEvent", "mouseup", true, true, payload)?;

            // `click` goes to the nearest common inclusive ancestor of the
            // press and release targets — pressing on one element and releasing
            // on another fires `click` on what contains both, not on either.
            if let Some(click_target) = common_ancestor(cx, pressed, Some(target)) {
                // A `PointerEvent`, per HTML's "fire a synthetic pointer event":
                // being a MouseEvent is what makes `dispatch_event` run the
                // activation behavior, so a synthesized click follows a link and
                // submits a form through exactly the path `.click()` uses.
                let payload = mouse_payload(cx, &input, Some(click_target), None, true);
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
                    let payload = mouse_payload(cx, &input, Some(click_target), None, false);
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
                    let payload = mouse_payload(cx, &input, Some(click_target), None, false);
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
