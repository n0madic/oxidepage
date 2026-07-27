//! `Event` implementation.

use std::cell::RefCell;
use std::rc::Rc;

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{EventData, target_to_js};

pub(crate) type EventRef = Rc<RefCell<EventData>>;

/// Reads the shared `EventInit` members from a dictionary value.
pub(crate) fn parse_event_init(cx: &BindCx<'_>, init: &JsValue) -> (bool, bool, bool) {
    match init {
        JsValue::Object(obj) => {
            let flag = |name: &str| cx.scope.get(obj, name).map(|v| v.truthy()).unwrap_or(false);
            (flag("bubbles"), flag("cancelable"), flag("composed"))
        }
        _ => (false, false, false),
    }
}

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    event_type: String,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let (bubbles, cancelable, composed) = parse_event_init(cx, &init);
    let mut data = EventData::new(event_type, bubbles, cancelable, composed);
    data.time_stamp = cx.now_ms();
    let (value, _) = cx.new_event_object("Event", data)?;
    Ok(value)
}

pub(crate) fn r#type(_cx: &BindCx<'_>, this: EventRef) -> Result<String, JsThrow> {
    Ok(this.borrow().event_type.clone())
}

pub(crate) fn target(cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    match this.borrow().target {
        Some(key) => target_to_js(cx, key),
        None => Ok(JsValue::Null),
    }
}

pub(crate) fn src_element(cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    target(cx, this)
}

pub(crate) fn current_target(cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    match this.borrow().current_target {
        Some(key) => target_to_js(cx, key),
        None => Ok(JsValue::Null),
    }
}

pub(crate) fn composed_path(cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    let path = this.borrow().path.clone();
    let mut items = Vec::with_capacity(path.len());
    for key in path {
        items.push(target_to_js(cx, key)?);
    }
    cx.scope
        .new_array(&items)
        .map(JsValue::Object)
        .map_err(JsThrow::from)
}

pub(crate) fn event_phase(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    Ok(f64::from(this.borrow().phase))
}

pub(crate) fn stop_propagation(_cx: &BindCx<'_>, this: EventRef) -> Result<(), JsThrow> {
    this.borrow_mut().stop_propagation = true;
    Ok(())
}

pub(crate) fn cancel_bubble(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    Ok(this.borrow().stop_propagation)
}

pub(crate) fn set_cancel_bubble(
    _cx: &BindCx<'_>,
    this: EventRef,
    value: bool,
) -> Result<(), JsThrow> {
    if value {
        this.borrow_mut().stop_propagation = true;
    }
    Ok(())
}

pub(crate) fn stop_immediate_propagation(_cx: &BindCx<'_>, this: EventRef) -> Result<(), JsThrow> {
    let mut ev = this.borrow_mut();
    ev.stop_propagation = true;
    ev.stop_immediate_propagation = true;
    Ok(())
}

pub(crate) fn bubbles(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    Ok(this.borrow().bubbles)
}

pub(crate) fn cancelable(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    Ok(this.borrow().cancelable)
}

pub(crate) fn return_value(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    Ok(!this.borrow().canceled)
}

pub(crate) fn set_return_value(
    cx: &BindCx<'_>,
    this: EventRef,
    value: bool,
) -> Result<(), JsThrow> {
    if !value {
        set_canceled_flag(cx, &this);
    }
    Ok(())
}

pub(crate) fn prevent_default(cx: &BindCx<'_>, this: EventRef) -> Result<(), JsThrow> {
    set_canceled_flag(cx, &this);
    Ok(())
}

/// Spec "set the canceled flag": marks the event canceled, but only if it is
/// cancelable *and* the in-passive-listener flag (§2.8) is not set. Shared by
/// `preventDefault()`, the legacy `returnValue` setter, and an event handler
/// that returns `false` — the single gate all three go through.
pub(crate) fn set_canceled_flag(cx: &BindCx<'_>, this: &EventRef) {
    let mut ev = this.borrow_mut();
    if !ev.cancelable {
        return;
    }
    if ev.in_passive_listener {
        drop(ev);
        cx.warn("Unable to preventDefault inside passive event listener invocation.");
        return;
    }
    ev.canceled = true;
}

pub(crate) fn default_prevented(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    Ok(this.borrow().canceled)
}

pub(crate) fn composed(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    Ok(this.borrow().composed)
}

pub(crate) fn is_trusted(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    Ok(this.borrow().is_trusted)
}

pub(crate) fn time_stamp(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    Ok(this.borrow().time_stamp)
}

pub(crate) fn init_event(
    _cx: &BindCx<'_>,
    this: EventRef,
    event_type: String,
    bubbles: bool,
    cancelable: bool,
) -> Result<(), JsThrow> {
    let mut ev = this.borrow_mut();
    if ev.dispatching {
        return Ok(());
    }
    ev.initialized = true;
    ev.stop_propagation = false;
    ev.stop_immediate_propagation = false;
    ev.canceled = false;
    ev.is_trusted = false;
    ev.target = None;
    ev.event_type = event_type;
    ev.bubbles = bubbles;
    ev.cancelable = cancelable;
    Ok(())
}
