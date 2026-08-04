//! `MouseEvent`, and the getters `WheelEvent`/`PointerEvent` inherit.
//!
//! All three share one [`MouseFields`] payload, because the latter two *are*
//! mouse events: `wheelEvent.clientX` must work, and a getter that
//! brand-checked on "is this exactly a MouseEvent" would break it.

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{MouseFields, UiKind};
use crate::imp::event::EventRef;
use crate::imp::ui_event::{
    member_f64, member_i32, member_node, member_u32, parse_ui_init, payload,
};

/// The mouse payload of `this`, or a `TypeError`. Shared by every getter in
/// this module and by `wheel_event`/`pointer_event`.
pub(crate) fn fields<T>(
    this: &EventRef,
    interface: &str,
    read: impl FnOnce(&MouseFields) -> T,
) -> Result<T, JsThrow> {
    payload(this, interface, |p| match &p.kind {
        UiKind::Mouse(m) => Some(read(m)),
        _ => None,
    })
}

/// Reads `MouseEventInit` (including everything it inherits) into a payload.
/// `wheel_event` and `pointer_event` call this and then add their own members.
pub(crate) fn parse_mouse_init(cx: &BindCx<'_>, init: &JsValue) -> MouseFields {
    MouseFields {
        screen_x: member_f64(cx, init, "screenX"),
        screen_y: member_f64(cx, init, "screenY"),
        client_x: member_f64(cx, init, "clientX"),
        client_y: member_f64(cx, init, "clientY"),
        // A constructed event has no target; `offsetX/Y` then track `pageX/Y`.
        offset: None,
        button: member_i32(cx, init, "button") as i16,
        buttons: member_u32(cx, init, "buttons") as u16,
        related: member_node(cx, init, "relatedTarget"),
        wheel: None,
        pointer: None,
    }
}

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    event_type: String,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let mouse = parse_mouse_init(cx, &init);
    let data = parse_ui_init(cx, event_type, &init, UiKind::Mouse(Box::new(mouse)))?;
    let (value, _) = cx.new_event_object("MouseEvent", data)?;
    Ok(value)
}

macro_rules! mouse_f64 {
    ($name:ident, $field:ident) => {
        pub(crate) fn $name(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
            fields(&this, "MouseEvent", |m| m.$field)
        }
    };
}

mouse_f64!(screen_x, screen_x);
mouse_f64!(screen_y, screen_y);
mouse_f64!(client_x, client_x);
mouse_f64!(client_y, client_y);

// `x`/`y` are aliases of `clientX`/`clientY`.
mouse_f64!(x, client_x);
mouse_f64!(y, client_y);

/// `offsetX`/`offsetY`: relative to the target's padding box when the event was
/// synthesized at one, and equal to `pageX`/`pageY` when it was constructed and
/// has no target.
pub(crate) fn offset_x(cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    match fields(&this, "MouseEvent", |m| m.offset)? {
        Some((x, _)) => Ok(x),
        None => page_x(cx, this),
    }
}

pub(crate) fn offset_y(cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    match fields(&this, "MouseEvent", |m| m.offset)? {
        Some((_, y)) => Ok(y),
        None => page_y(cx, this),
    }
}

/// `pageX`/`pageY` are client coordinates plus the document scroll — computed
/// at read time, because a listener may have scrolled the page since dispatch
/// began and the spec defines them against the current scroll position.
pub(crate) fn page_x(cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    let client = fields(&this, "MouseEvent", |m| m.client_x)?;
    Ok(client + f64::from(cx.state.layout.borrow().viewport_scroll().x))
}

pub(crate) fn page_y(cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    let client = fields(&this, "MouseEvent", |m| m.client_y)?;
    Ok(client + f64::from(cx.state.layout.borrow().viewport_scroll().y))
}

pub(crate) fn button(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    fields(&this, "MouseEvent", |m| f64::from(m.button))
}

pub(crate) fn buttons(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    fields(&this, "MouseEvent", |m| f64::from(m.buttons))
}

/// The stored wrapper itself, so `e.relatedTarget === theNodeThatWasPassedIn`.
/// Re-minting it from an id would be both slower and, for a node freed since,
/// a throw where the spec wants the object back.
pub(crate) fn related_target(cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    // Resolved through **this** world's wrapper cache. `node_to_js` is a cache
    // lookup, so `e.relatedTarget === node` still holds within a world; across
    // worlds the objects are necessarily distinct (ADR-0033 D5).
    let related = fields(&this, "MouseEvent", |m| {
        m.related.as_ref().map(crate::events::PinnedNode::id)
    })?;
    cx.opt_node_to_js(related)
}

pub(crate) fn ctrl_key(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    payload(&this, "MouseEvent", |p| Some(p.modifiers.ctrl))
}

pub(crate) fn shift_key(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    payload(&this, "MouseEvent", |p| Some(p.modifiers.shift))
}

pub(crate) fn alt_key(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    payload(&this, "MouseEvent", |p| Some(p.modifiers.alt))
}

pub(crate) fn meta_key(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    payload(&this, "MouseEvent", |p| Some(p.modifiers.meta))
}

pub(crate) fn get_modifier_state(
    _cx: &BindCx<'_>,
    this: EventRef,
    key: String,
) -> Result<bool, JsThrow> {
    payload(&this, "MouseEvent", |p| Some(p.modifiers.state(&key)))
}
