//! `PointerEvent`. Like `WheelEvent`, a mouse event with extra members rather
//! than a payload of its own — modern UI libraries (drag helpers, Radix,
//! Floating UI) listen for `pointerdown` and read `clientX` off the same
//! object.

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{PointerFields, UiKind};
use crate::imp::event::EventRef;
use crate::imp::mouse_event::parse_mouse_init;
use crate::imp::ui_event::{
    member_bool, member_f64, member_i32, member_string, parse_ui_init, payload,
};

fn pointer<T>(this: &EventRef, read: impl FnOnce(&PointerFields) -> T) -> Result<T, JsThrow> {
    payload(this, "PointerEvent", |p| match &p.kind {
        UiKind::Mouse(m) => m.pointer.as_ref().map(read),
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
    mouse.pointer = Some(PointerFields {
        pointer_id: member_i32(cx, &init, "pointerId"),
        width: member_f64(cx, &init, "width"),
        height: member_f64(cx, &init, "height"),
        pressure: member_f64(cx, &init, "pressure"),
        pointer_type: member_string(cx, &init, "pointerType"),
        is_primary: member_bool(cx, &init, "isPrimary"),
    });
    let data = parse_ui_init(cx, event_type, &init, UiKind::Mouse(Box::new(mouse)))?;
    let (value, _) = cx.new_event_object("PointerEvent", data)?;
    Ok(value)
}

pub(crate) fn pointer_id(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    pointer(&this, |p| f64::from(p.pointer_id))
}

pub(crate) fn width(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    pointer(&this, |p| p.width)
}

pub(crate) fn height(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    pointer(&this, |p| p.height)
}

pub(crate) fn pressure(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    pointer(&this, |p| p.pressure)
}

pub(crate) fn pointer_type(_cx: &BindCx<'_>, this: EventRef) -> Result<String, JsThrow> {
    pointer(&this, |p| p.pointer_type.clone())
}

pub(crate) fn is_primary(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    pointer(&this, |p| p.is_primary)
}

/// The pointer half of a synthesized mouse sequence: one primary mouse
/// pointer, which is all a headless engine has.
pub(crate) fn mouse_pointer() -> PointerFields {
    PointerFields {
        pointer_id: 1,
        width: 1.0,
        height: 1.0,
        pressure: 0.5,
        pointer_type: "mouse".to_owned(),
        is_primary: true,
    }
}
