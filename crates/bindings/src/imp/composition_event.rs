//! `CompositionEvent`.
//!
//! The interface exists and is constructible — test tooling builds one, and
//! `Event-subclasses-constructors.html` checks it — but the engine never
//! *generates* a composition: there is no IME in a headless process. That is a
//! data carrier that behaves correctly, not a stub that lies (P6).

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::UiKind;
use crate::imp::event::EventRef;
use crate::imp::ui_event::{member_string, parse_ui_init, payload};

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    event_type: String,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let data = member_string(cx, &init, "data");
    let event = parse_ui_init(cx, event_type, &init, UiKind::Composition { data })?;
    let (value, _) = cx.new_event_object("CompositionEvent", event)?;
    Ok(value)
}

pub(crate) fn data(_cx: &BindCx<'_>, this: EventRef) -> Result<String, JsThrow> {
    payload(&this, "CompositionEvent", |p| match &p.kind {
        UiKind::Composition { data } => Some(data.clone()),
        _ => None,
    })
}
