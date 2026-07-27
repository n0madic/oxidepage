//! `InputEvent`: `beforeinput` and `input`.
//!
//! `inputType` is what tells a framework *how* the value changed
//! (`insertText`, `deleteContentBackward`, …); React's controlled-input logic
//! and every rich-text editor branch on it, so a blank one is not a
//! simplification, it is a wrong answer.

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{InputFields, UiKind};
use crate::imp::event::EventRef;
use crate::imp::ui_event::{
    member_bool, member_nullable_string, member_string, parse_ui_init, payload,
};

fn fields<T>(this: &EventRef, read: impl FnOnce(&InputFields) -> T) -> Result<T, JsThrow> {
    payload(this, "InputEvent", |p| match &p.kind {
        UiKind::Input(i) => Some(read(i)),
        _ => None,
    })
}

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    event_type: String,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let input = InputFields {
        data: member_nullable_string(cx, &init, "data"),
        is_composing: member_bool(cx, &init, "isComposing"),
        input_type: member_string(cx, &init, "inputType"),
    };
    let data = parse_ui_init(cx, event_type, &init, UiKind::Input(Box::new(input)))?;
    let (value, _) = cx.new_event_object("InputEvent", data)?;
    Ok(value)
}

pub(crate) fn data(_cx: &BindCx<'_>, this: EventRef) -> Result<Option<String>, JsThrow> {
    fields(&this, |i| i.data.clone())
}

pub(crate) fn is_composing(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    fields(&this, |i| i.is_composing)
}

pub(crate) fn input_type(_cx: &BindCx<'_>, this: EventRef) -> Result<String, JsThrow> {
    fields(&this, |i| i.input_type.clone())
}
