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
/// the event past a frame's removal sees. When the sender is the reading
/// realm's own context it is that realm's `window` itself, as `window.parent`
/// of a top-level page is — so `initMessageEvent(…, window)` reads back as
/// `event.source === window`, not as a second object standing for it.
pub(crate) fn source(cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    let source = this.borrow().message.as_ref().and_then(|m| m.source);
    match source {
        Some(frame) if frame == cx.state.frame.frame() => cx.global_value(),
        Some(frame) if cx.frame_state(frame).is_some() => cx.new_frame_proxy(frame),
        _ => Ok(JsValue::Null),
    }
}

/// `initMessageEvent`: the legacy initializer `createEvent("MessageEvent")` is
/// useless without. Like every `init*Event` it is a no-op on an event being
/// dispatched, and it replaces every member it has an argument for.
///
/// Arguments are converted first, as WebIDL does before the method body runs,
/// so a bad `source` throws without touching the event. `data` is stored the
/// way the constructor stores it: serialized, so every world that receives
/// the event gets its own copy (see the module docs).
#[allow(clippy::too_many_arguments)]
pub(crate) fn init_message_event(
    cx: &BindCx<'_>,
    this: EventRef,
    event_type: String,
    bubbles: bool,
    cancelable: bool,
    data: JsValue,
    origin: String,
    last_event_id: String,
    source: JsValue,
) -> Result<(), JsThrow> {
    // Brand check: only a `MessageEvent` carries a message payload.
    if this.borrow().message.is_none() {
        return Err(JsThrow::Type("receiver is not a MessageEvent".to_owned()));
    }
    let source = message_source(cx, &source)?;
    // `optional any data = null`: an explicit `undefined` takes the default.
    let data = if data.is_undefined() {
        JsValue::Null
    } else {
        data
    };
    let detail = match crate::imp::history::serialize_for_event(cx, &data)? {
        Some(text) => EventDetail::Serialized(text),
        None => EventDetail::None,
    };
    if this.borrow().dispatching {
        return Ok(());
    }
    crate::imp::event::init_event(cx, this.clone(), event_type, bubbles, cancelable)?;
    let mut ev = this.borrow_mut();
    ev.detail = detail;
    ev.message = Some(Box::new(MessagePayload {
        origin,
        source,
        last_event_id,
    }));
    Ok(())
}

/// `MessageEventSource?` as this engine can represent it: `null`, this realm's
/// own `window`, or a `WindowProxy` for one of this page's frames — stored as
/// the frame, so each realm that reads `source` mints its own proxy.
///
/// Anything else is a `TypeError`, as the union conversion is. That includes
/// a popup's `WindowProxy` from `window.open`: it lives in another page, which
/// a `MessagePayload` cannot name, and answering `null` for it would be a
/// silently wrong value.
fn message_source(
    cx: &BindCx<'_>,
    value: &JsValue,
) -> Result<Option<oxidepage_base::FrameId>, JsThrow> {
    if value.is_nullish() {
        return Ok(None);
    }
    if cx.scope.strict_equals(value, &cx.global_value()?) {
        return Ok(Some(cx.state.frame.frame()));
    }
    match cx.this_window_proxy(value).as_deref() {
        Ok(crate::window_open::WindowProxyData::Frame(frame)) => Ok(Some(*frame)),
        _ => Err(JsThrow::Type(
            "initMessageEvent: source is not a window of this page".to_owned(),
        )),
    }
}
