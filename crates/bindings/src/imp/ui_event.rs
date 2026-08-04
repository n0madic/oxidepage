//! `UIEvent`, and the shared init-dictionary parsing for the whole UI event
//! family.
//!
//! WebIDL dictionary *inheritance* (`MouseEventInit : EventModifierInit :
//! UIEventInit : EventInit`) is not something the code generator knows about —
//! a dictionary is a passthrough type and the glue hands the imp a raw value.
//! So the inheritance is expressed here, as a chain of readers each of which
//! calls the one above it. That is why every subinterface constructor starts
//! with [`parse_ui_init`].

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{EventData, Modifiers, UiKind, UiPayload};
use crate::imp::event::{EventRef, parse_event_init};

/// Reads a dictionary member, or `None` when it is absent — which for WebIDL
/// means missing *or* `undefined`, both of which take the declared default.
/// An explicit `null` is a value and is returned; each typed reader below
/// decides what it means.
pub(crate) fn member(cx: &BindCx<'_>, init: &JsValue, name: &str) -> Option<JsValue> {
    let JsValue::Object(obj) = init else {
        return None;
    };
    cx.scope.get(obj, name).ok().filter(|v| !v.is_undefined())
}

/// A `double` member. Non-finite coerces to 0, as WebIDL's `double` (not
/// `unrestricted double`) rejects NaN/Infinity — every member here is the
/// restricted kind.
pub(crate) fn member_f64(cx: &BindCx<'_>, init: &JsValue, name: &str) -> f64 {
    member(cx, init, name)
        .and_then(|v| cx.scope.coerce_number(&v).ok())
        .filter(|n| n.is_finite())
        .unwrap_or(0.0)
}

pub(crate) fn member_bool(cx: &BindCx<'_>, init: &JsValue, name: &str) -> bool {
    member(cx, init, name).is_some_and(|v| v.truthy())
}

/// A non-nullable `DOMString` member: `null` stringifies to `"null"`, per the
/// ECMAScript conversion WebIDL applies.
pub(crate) fn member_string(cx: &BindCx<'_>, init: &JsValue, name: &str) -> String {
    member(cx, init, name)
        .and_then(|v| cx.scope.coerce_string(&v).ok())
        .unwrap_or_default()
}

/// A `DOMString?` member, where `null` is a distinct value from absent —
/// `InputEvent.data` is null for a deletion and a string for an insertion.
pub(crate) fn member_nullable_string(
    cx: &BindCx<'_>,
    init: &JsValue,
    name: &str,
) -> Option<String> {
    let value = member(cx, init, name)?;
    if matches!(value, JsValue::Null) {
        return None;
    }
    cx.scope.coerce_string(&value).ok()
}

/// A `long`/`unsigned long` dictionary member: ECMAScript ToInt32 semantics, so
/// a fractional or out-of-range value wraps rather than saturating.
pub(crate) fn member_i32(cx: &BindCx<'_>, init: &JsValue, name: &str) -> i32 {
    let n = member_f64(cx, init, name);
    n as i64 as u32 as i32
}

pub(crate) fn member_u32(cx: &BindCx<'_>, init: &JsValue, name: &str) -> u32 {
    member_i32(cx, init, name) as u32
}

/// An `EventTarget?` dictionary member, kept as a **pinned node id**. Only
/// nodes can be a related target here; anything else (the Window, a bare
/// `EventTarget`) reads as absent.
///
/// The pin — not a wrapper — is what the payload stores, so the node cannot be
/// collected out from under an event a listener retained *and* the payload
/// stays world-neutral; see [`crate::events::MouseFields::related`].
pub(crate) fn member_node(
    cx: &BindCx<'_>,
    init: &JsValue,
    name: &str,
) -> Option<crate::events::PinnedNode> {
    let value = member(cx, init, name)?;
    if value.is_nullish() {
        return None;
    }
    let id = cx.this_node(&value).ok()?;
    Some(crate::events::PinnedNode::new(&cx.state.dom, id))
}

/// `EventModifierInit`'s four flags.
pub(crate) fn parse_modifiers(cx: &BindCx<'_>, init: &JsValue) -> Modifiers {
    Modifiers {
        ctrl: member_bool(cx, init, "ctrlKey"),
        shift: member_bool(cx, init, "shiftKey"),
        alt: member_bool(cx, init, "altKey"),
        meta: member_bool(cx, init, "metaKey"),
    }
}

/// Builds the `EventData` common to the whole family: the `EventInit` members,
/// the `UIEventInit` members, and the modifier flags.
///
/// Fallible only because of `view`: the member is declared `Window?`, so
/// anything that is not the Window is a `TypeError` from the WebIDL
/// conversion. The generator types it `any` (it does not return interface-typed
/// attributes), which means the check has to live here — and it is observable,
/// so it is not optional.
pub(crate) fn parse_ui_init(
    cx: &BindCx<'_>,
    event_type: String,
    init: &JsValue,
    kind: UiKind,
) -> Result<EventData, JsThrow> {
    let (bubbles, cancelable, composed) = parse_event_init(cx, init);
    let mut data = EventData::new(event_type, bubbles, cancelable, composed);
    data.time_stamp = cx.now_ms();
    let mut payload = UiPayload::new(kind);
    payload.detail = member_i32(cx, init, "detail");
    payload.has_view = parse_view(cx, init)?;
    payload.modifiers = parse_modifiers(cx, init);
    data.ui = Some(Box::new(payload));
    Ok(data)
}

/// Whether `view` was set to the Window. There is one browsing context, so the
/// only legal non-null value is the global itself.
fn parse_view(cx: &BindCx<'_>, init: &JsValue) -> Result<bool, JsThrow> {
    let Some(value) = member(cx, init, "view").filter(|v| !v.is_nullish()) else {
        return Ok(false);
    };
    let global = JsValue::Object(cx.with_js(|js| js.global.clone())?);
    if cx.scope.strict_equals(&value, &global) {
        Ok(true)
    } else {
        Err(JsThrow::Type(
            "UIEventInit: `view` is not a Window".to_owned(),
        ))
    }
}

/// The payload of a UI event, or a `TypeError` when the receiver is a plain
/// `Event` (which every UI getter must reject).
pub(crate) fn payload<T>(
    this: &EventRef,
    interface: &str,
    read: impl FnOnce(&UiPayload) -> Option<T>,
) -> Result<T, JsThrow> {
    let ev = this.borrow();
    ev.ui
        .as_deref()
        .and_then(read)
        .ok_or_else(|| JsThrow::Type(format!("receiver is not a {interface}")))
}

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    event_type: String,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let data = parse_ui_init(cx, event_type, &init, UiKind::Plain)?;
    let (value, _) = cx.new_event_object("UIEvent", data)?;
    Ok(value)
}

pub(crate) fn detail(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    payload(&this, "UIEvent", |p| Some(f64::from(p.detail)))
}

/// `view` is the Window or null — there is one browsing context, so the
/// payload stores only whether it was set.
pub(crate) fn view(cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    let has_view = payload(&this, "UIEvent", |p| Some(p.has_view))?;
    if !has_view {
        return Ok(JsValue::Null);
    }
    let global = cx.with_js(|js| js.global.clone())?;
    Ok(JsValue::Object(global))
}

pub(crate) fn init_ui_event(
    cx: &BindCx<'_>,
    this: EventRef,
    event_type: String,
    bubbles: bool,
    cancelable: bool,
    view: JsValue,
    detail: i32,
) -> Result<(), JsThrow> {
    super::event::init_event(cx, this.clone(), event_type, bubbles, cancelable)?;
    let mut ev = this.borrow_mut();
    if ev.dispatching {
        return Ok(());
    }
    let payload = ev
        .ui
        .get_or_insert_with(|| Box::new(UiPayload::new(UiKind::Plain)));
    payload.has_view = !view.is_nullish();
    payload.detail = detail;
    Ok(())
}
