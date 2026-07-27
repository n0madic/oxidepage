//! `ProgressEvent` — the event interface `XMLHttpRequest` reports transfer
//! progress with (`loadstart`, `progress`, `load`, `error`, `abort`,
//! `timeout`, `loadend`).
//!
//! It inherits from `Event`, not from `UIEvent`, but its three members live in
//! the shared [`crate::events::EventData::ui`] payload slot as
//! [`UiKind::Progress`]: the payload variant *is* the brand, so
//! `ProgressEvent.prototype.loaded` called on a plain `Event` throws instead of
//! reading a field that happens to exist (ADR-0024).

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{EventData, UiKind, UiPayload};
use crate::imp::event::{EventRef, parse_event_init};
use crate::imp::ui_event::{member_bool, member_f64, payload};

/// An `unsigned long long` dictionary member. WebIDL clamps a negative or
/// non-finite value to 0 and truncates the fraction; every value a transfer can
/// produce is far below 2^53, so an `f64` carries it exactly.
fn member_u64(cx: &BindCx<'_>, init: &JsValue, name: &str) -> f64 {
    let n = member_f64(cx, init, name);
    if n > 0.0 { n.trunc() } else { 0.0 }
}

/// Builds the `EventData` for a progress event with the given counters. Shared
/// with the XHR dispatch path, which fires these events without an init
/// dictionary.
pub(crate) fn event_data(
    event_type: &str,
    length_computable: bool,
    loaded: f64,
    total: f64,
) -> EventData {
    EventData::new(
        event_type.to_owned(),
        /* bubbles */ false,
        /* cancelable */ false,
        /* composed */ false,
    )
    .with_ui(UiPayload::new(UiKind::Progress {
        length_computable,
        loaded,
        total,
    }))
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
    data.ui = Some(Box::new(UiPayload::new(UiKind::Progress {
        length_computable: member_bool(cx, &init, "lengthComputable"),
        loaded: member_u64(cx, &init, "loaded"),
        total: member_u64(cx, &init, "total"),
    })));
    let (value, _) = cx.new_event_object("ProgressEvent", data)?;
    Ok(value)
}

fn progress<T>(this: &EventRef, read: impl FnOnce(bool, f64, f64) -> T) -> Result<T, JsThrow> {
    payload(this, "ProgressEvent", |p| match &p.kind {
        UiKind::Progress {
            length_computable,
            loaded,
            total,
        } => Some(read(*length_computable, *loaded, *total)),
        _ => None,
    })
}

pub(crate) fn length_computable(_cx: &BindCx<'_>, this: EventRef) -> Result<bool, JsThrow> {
    progress(&this, |computable, _, _| computable)
}

pub(crate) fn loaded(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    progress(&this, |_, loaded, _| loaded)
}

pub(crate) fn total(_cx: &BindCx<'_>, this: EventRef) -> Result<f64, JsThrow> {
    progress(&this, |_, _, total| total)
}
