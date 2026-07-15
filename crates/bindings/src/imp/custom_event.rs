//! `CustomEvent` implementation.

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::EventData;
use crate::imp::event::{EventRef, parse_event_init};

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    event_type: String,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let (bubbles, cancelable, composed) = parse_event_init(cx, &init);
    let mut data = EventData::new(event_type, bubbles, cancelable, composed);
    data.time_stamp = cx.now_ms();
    if let JsValue::Object(obj) = &init
        && let Ok(detail) = cx.scope.get(obj, "detail")
        && !detail.is_undefined()
    {
        data.detail = detail;
    }
    let (value, _) = cx.new_event_object("CustomEvent", data)?;
    Ok(value)
}

pub(crate) fn detail(_cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    Ok(this.borrow().detail.clone())
}

pub(crate) fn init_custom_event(
    cx: &BindCx<'_>,
    this: EventRef,
    event_type: String,
    bubbles: bool,
    cancelable: bool,
    detail: JsValue,
) -> Result<(), JsThrow> {
    super::event::init_event(cx, this.clone(), event_type, bubbles, cancelable)?;
    let mut ev = this.borrow_mut();
    if !ev.dispatching {
        ev.detail = detail;
    }
    Ok(())
}
