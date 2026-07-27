//! `XMLHttpRequestEventTarget` — the base both `XMLHttpRequest` and
//! `XMLHttpRequestUpload` inherit from, carrying the seven handler properties
//! they share.
//!
//! The handlers are stored in the shared `event_handlers` registry keyed by the
//! receiver's [`EventTargetKey::Host`], not on any per-object struct — which is
//! what puts them on the same footing as `addEventListener` registrations, so
//! `invoke_listeners` runs them and listener options work from either.
//!
//! They are declared `any` rather than `EventHandler` in the IDL on purpose:
//! an `EventHandler` attribute also joins `EVENT_HANDLER_TYPES`, the list of
//! event-handler *content* attributes, and `<div ontimeout>` /
//! `<div onreadystatechange>` are not handlers in HTML.

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::EventTargetKey;

/// Reads a handler slot. Absent reads as `null`.
pub(crate) fn get(cx: &BindCx<'_>, key: EventTargetKey, event_type: &str) -> JsValue {
    cx.state
        .event_handlers
        .borrow()
        .get(&(key, event_type.to_owned()))
        .cloned()
        .unwrap_or(JsValue::Null)
}

/// Writes a handler slot; a nullish value removes it.
pub(crate) fn set(cx: &BindCx<'_>, key: EventTargetKey, event_type: &str, value: JsValue) {
    let slot = (key, event_type.to_owned());
    let mut handlers = cx.state.event_handlers.borrow_mut();
    if value.is_nullish() {
        handlers.remove(&slot);
    } else {
        handlers.insert(slot, value);
    }
}

macro_rules! handler {
    ($getter:ident, $setter:ident, $event_type:literal) => {
        pub(crate) fn $getter(cx: &BindCx<'_>, this: EventTargetKey) -> Result<JsValue, JsThrow> {
            Ok(get(cx, this, $event_type))
        }
        pub(crate) fn $setter(
            cx: &BindCx<'_>,
            this: EventTargetKey,
            value: JsValue,
        ) -> Result<(), JsThrow> {
            set(cx, this, $event_type, value);
            Ok(())
        }
    };
}

handler!(onloadstart, set_onloadstart, "loadstart");
handler!(onprogress, set_onprogress, "progress");
handler!(onabort, set_onabort, "abort");
handler!(onerror, set_onerror, "error");
handler!(onload, set_onload, "load");
handler!(ontimeout, set_ontimeout, "timeout");
handler!(onloadend, set_onloadend, "loadend");
