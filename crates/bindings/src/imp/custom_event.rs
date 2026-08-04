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
    let mut owns_value = false;
    if let JsValue::Object(obj) = &init
        && let Ok(detail) = cx.scope.get(obj, "detail")
        && !detail.is_undefined()
    {
        data.detail = crate::events::EventDetail::Value {
            world: cx.state.id,
            value: detail,
        };
        owns_value = true;
    }
    let (value, data) = cx.new_event_object("CustomEvent", data)?;
    if owns_value {
        // The payload is shared with every world that wraps this event, so the
        // world that owns the value has to be able to find it again at
        // teardown (ADR-0033 D5).
        cx.own_event_value(&data);
    }
    Ok(value)
}

/// `null` when the event was constructed in another world.
///
/// That is the isolation boundary doing its job, not a gap: the value is a
/// live object of the world that made it, and no value can cross (ADR-0033
/// D5). Chrome behaves the same way.
pub(crate) fn detail(cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    Ok(this.borrow().detail.value_in(cx.state.id))
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
    let mut owns_value = false;
    {
        let mut ev = this.borrow_mut();
        if !ev.dispatching {
            ev.detail = crate::events::EventDetail::Value {
                world: cx.state.id,
                value: detail,
            };
            owns_value = true;
        }
    }
    if owns_value {
        cx.own_event_value(&this);
    }
    Ok(())
}
