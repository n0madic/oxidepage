//! `KeyboardEvent`.
//!
//! `charCode`/`keyCode`/`which` are legacy and deprecated, and are implemented
//! anyway: jQuery and every hotkey library still read `which`, so omitting them
//! would not be honesty about an unimplemented feature — it would be a
//! `KeyboardEvent` that real code cannot use.

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{KeyboardFields, UiKind};
use crate::imp::event::EventRef;
use crate::imp::ui_event::{member_bool, member_string, member_u32, parse_ui_init, payload};

fn fields<T>(this: &EventRef, read: impl FnOnce(&KeyboardFields) -> T) -> Result<T, JsThrow> {
    payload(this, "KeyboardEvent", |p| match &p.kind {
        UiKind::Keyboard(k) => Some(read(k)),
        _ => None,
    })
}

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    event_type: String,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let keyboard = KeyboardFields {
        key: member_string(cx, &init, "key"),
        code: member_string(cx, &init, "code"),
        location: member_u32(cx, &init, "location"),
        repeat: member_bool(cx, &init, "repeat"),
        is_composing: member_bool(cx, &init, "isComposing"),
        char_code: member_u32(cx, &init, "charCode"),
        key_code: member_u32(cx, &init, "keyCode"),
    };
    let data = parse_ui_init(cx, event_type, &init, UiKind::Keyboard(Box::new(keyboard)))?;
    let (value, _) = cx.new_event_object("KeyboardEvent", data)?;
    Ok(value)
}

pub(crate) fn key(_cx: &BindCx<'_>, this: EventRef) -> Result<String, JsThrow> {
    fields(&this, |k| k.key.clone())
}

pub(crate) fn code(_cx: &BindCx<'_>, this: EventRef) -> Result<String, JsThrow> {
    fields(&this, |k| k.code.clone())
}

pub(crate) fn location(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    fields(&this, |k| f64::from(k.location))
}

pub(crate) fn repeat(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    fields(&this, |k| k.repeat)
}

pub(crate) fn is_composing(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    fields(&this, |k| k.is_composing)
}

pub(crate) fn char_code(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    fields(&this, |k| f64::from(k.char_code))
}

pub(crate) fn key_code(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    fields(&this, |k| f64::from(k.key_code))
}

/// `which` is `charCode` when there is one (a `keypress`), else `keyCode`.
pub(crate) fn which(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    fields(&this, |k| {
        f64::from(if k.char_code != 0 {
            k.char_code
        } else {
            k.key_code
        })
    })
}

pub(crate) fn ctrl_key(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    payload(&this, "KeyboardEvent", |p| Some(p.modifiers.ctrl))
}

pub(crate) fn shift_key(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    payload(&this, "KeyboardEvent", |p| Some(p.modifiers.shift))
}

pub(crate) fn alt_key(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    payload(&this, "KeyboardEvent", |p| Some(p.modifiers.alt))
}

pub(crate) fn meta_key(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    payload(&this, "KeyboardEvent", |p| Some(p.modifiers.meta))
}

pub(crate) fn get_modifier_state(
    _cx: &BindCx<'_>,
    this: EventRef,
    key: String,
) -> Result<bool, JsThrow> {
    payload(&this, "KeyboardEvent", |p| Some(p.modifiers.state(&key)))
}
