//! `WheelEvent`. A mouse event with scroll deltas — every `MouseEvent` getter
//! works on it, which is why the payload is a [`MouseFields`] carrying an
//! optional [`WheelFields`] rather than a variant of its own.

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{UiKind, WheelFields};
use crate::imp::event::EventRef;
use crate::imp::mouse_event::parse_mouse_init;
use crate::imp::ui_event::{member_f64, member_u32, parse_ui_init, payload};

fn wheel<T>(this: &EventRef, read: impl FnOnce(&WheelFields) -> T) -> Result<T, JsThrow> {
    payload(this, "WheelEvent", |p| match &p.kind {
        UiKind::Mouse(m) => m.wheel.as_ref().map(read),
        _ => None,
    })
}

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    event_type: String,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let mut mouse = parse_mouse_init(cx, &init);
    mouse.wheel = Some(WheelFields {
        delta_x: member_f64(cx, &init, "deltaX"),
        delta_y: member_f64(cx, &init, "deltaY"),
        delta_z: member_f64(cx, &init, "deltaZ"),
        delta_mode: member_u32(cx, &init, "deltaMode"),
    });
    let data = parse_ui_init(cx, event_type, &init, UiKind::Mouse(Box::new(mouse)))?;
    let (value, _) = cx.new_event_object("WheelEvent", data)?;
    Ok(value)
}

pub(crate) fn delta_x(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    wheel(&this, |w| w.delta_x)
}

pub(crate) fn delta_y(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    wheel(&this, |w| w.delta_y)
}

pub(crate) fn delta_z(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    wheel(&this, |w| w.delta_z)
}

pub(crate) fn delta_mode(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    wheel(&this, |w| f64::from(w.delta_mode))
}
