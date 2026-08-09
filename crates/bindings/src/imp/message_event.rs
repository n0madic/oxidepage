//! `MessageEvent`: what `postMessage` delivers (ADR-0035 D4).
//!
//! The body rides in [`EventDetail::Serialized`], the same slot
//! `PopStateEvent.state` uses and for the same reason — every world that
//! receives the event materializes its own copy rather than one world seeing
//! the value and the rest seeing `null`. The rest of the fields live in
//! [`MessagePayload`].

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{EventData, EventDetail, MessagePayload};
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
    let mut payload = MessagePayload {
        origin: String::new(),
        source: None,
        last_event_id: String::new(),
    };
    if let JsValue::Object(obj) = &init {
        if let Ok(value) = cx.scope.get(obj, "data")
            && !value.is_undefined()
        {
            data.detail = match crate::imp::history::serialize_for_event(cx, &value)? {
                Some(text) => EventDetail::Serialized(text),
                None => EventDetail::None,
            };
        }
        if let Ok(value) = cx.scope.get(obj, "origin")
            && !value.is_undefined()
        {
            payload.origin = cx.scope.coerce_string(&value)?;
        }
        if let Ok(value) = cx.scope.get(obj, "lastEventId")
            && !value.is_undefined()
        {
            payload.last_event_id = cx.scope.coerce_string(&value)?;
        }
    }
    data.message = Some(Box::new(payload));
    let (value, _) = cx.new_event_object("MessageEvent", data)?;
    Ok(value)
}

pub(crate) fn data(cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    let serialized = match &this.borrow().detail {
        EventDetail::Serialized(text) => Some(text.clone()),
        _ => None,
    };
    crate::imp::history::deserialize_state(cx, serialized.as_deref())
}

pub(crate) fn origin(_cx: &BindCx<'_>, this: EventRef) -> Result<String, JsThrow> {
    Ok(this
        .borrow()
        .message
        .as_ref()
        .map(|m| m.origin.clone())
        .unwrap_or_default())
}

pub(crate) fn last_event_id(_cx: &BindCx<'_>, this: EventRef) -> Result<String, JsThrow> {
    Ok(this
        .borrow()
        .message
        .as_ref()
        .map(|m| m.last_event_id.clone())
        .unwrap_or_default())
}

/// The sending context, as a `WindowProxy` minted **in this realm**.
///
/// `null` once the sender's context is gone, which is what a receiver holding
/// the event past a frame's removal sees.
pub(crate) fn source(cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    let source = this.borrow().message.as_ref().and_then(|m| m.source);
    match source {
        Some(frame) if cx.frame_state(frame).is_some() => cx.new_frame_proxy(frame),
        _ => Ok(JsValue::Null),
    }
}
