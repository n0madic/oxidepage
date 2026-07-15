//! `EventTarget` implementation.

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{self, EventTargetKey};

/// `new EventTarget()`. The listeners live in the shared registry like any other
/// target's; all this allocates is an identity to key them by.
pub(crate) fn constructor(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.new_event_target()
}

/// The flattened `AddEventListenerOptions`.
struct AddOptions {
    capture: bool,
    once: bool,
    passive: bool,
    /// The slab key of the `AbortSignal` that will remove this listener.
    signal: Option<u64>,
}

/// Flattens the `(AddEventListenerOptions or boolean)` union for
/// `addEventListener`. Every member here is add-only, so its getter (if any, on
/// a dictionary-like object) must be observed *before* the null-callback
/// early-out (WPT: "Supports passive option on addEventListener only").
///
/// `signal` is declared `AbortSignal` — non-nullable — so an explicit `null`
/// fails WebIDL conversion and throws, while an absent/`undefined` one is simply
/// no signal (WPT: "Passing null as the signal should throw").
fn flatten_add_options(cx: &BindCx<'_>, options: &JsValue) -> Result<AddOptions, JsThrow> {
    let JsValue::Object(obj) = options else {
        return Ok(AddOptions {
            capture: options.truthy(),
            once: false,
            passive: false,
            signal: None,
        });
    };
    let flag = |name: &str| cx.scope.get(obj, name).map(|v| v.truthy()).unwrap_or(false);
    let capture = flag("capture");
    let once = flag("once");
    let passive = flag("passive");

    let signal = match cx.scope.get(obj, "signal") {
        Ok(value) if !value.is_undefined() => Some(cx.abort_signal_key(&value)?),
        _ => None,
    };
    Ok(AddOptions {
        capture,
        once,
        passive,
        signal,
    })
}

/// Flattens the `(EventListenerOptions or boolean)` union for
/// `removeEventListener`. Only `capture` is a member of
/// `EventListenerOptions` — `once`/`passive` must NOT be read here (WPT:
/// "removeEventListener supports the passive option when it should not").
fn flatten_remove_options(cx: &BindCx<'_>, options: &JsValue) -> bool {
    match options {
        JsValue::Object(obj) => cx
            .scope
            .get(obj, "capture")
            .map(|v| v.truthy())
            .unwrap_or(false),
        other => other.truthy(),
    }
}

pub(crate) fn add_event_listener(
    cx: &BindCx<'_>,
    this: EventTargetKey,
    event_type: String,
    callback: JsValue,
    options: JsValue,
) -> Result<(), JsThrow> {
    // Options are converted (getters observed, `signal` type-checked) before the
    // null-callback early-out — see `flatten_add_options`.
    let opts = flatten_add_options(cx, &options)?;
    if callback.is_nullish() {
        return Ok(());
    }
    // An already-aborted signal means the listener is removed the instant it is
    // added, which the spec short-circuits into never adding it.
    if opts.signal.is_some_and(|key| cx.abort_signal_aborted(key)) {
        return Ok(());
    }
    // Snapshot the existing callbacks and run the `===` dedup check with the
    // registry unborrowed — `strict_equals` re-enters JS.
    let existing = cx
        .state
        .listeners
        .borrow()
        .matching(this, &event_type, opts.capture);
    if existing
        .iter()
        .any(|(_, cb)| cx.scope.strict_equals(cb, &callback))
    {
        // Spec: an equivalent (type, callback, capture) listener already
        // exists — do nothing. `passive`/`once`/`signal` are not part of identity
        // and do not update the existing listener's flags.
        return Ok(());
    }
    cx.state.listeners.borrow_mut().insert(
        this,
        event_type,
        callback,
        opts.capture,
        opts.once,
        opts.passive,
        opts.signal,
    );
    Ok(())
}

pub(crate) fn remove_event_listener(
    cx: &BindCx<'_>,
    this: EventTargetKey,
    event_type: String,
    callback: JsValue,
    options: JsValue,
) -> Result<(), JsThrow> {
    if callback.is_nullish() {
        return Ok(());
    }
    let capture = flatten_remove_options(cx, &options);
    // Find the matching listener with the registry unborrowed (the `===`
    // comparison re-enters JS), then remove it by id.
    let existing = cx
        .state
        .listeners
        .borrow()
        .matching(this, &event_type, capture);
    let matched = existing
        .iter()
        .find(|(_, cb)| cx.scope.strict_equals(cb, &callback))
        .map(|(id, _)| *id);
    if let Some(id) = matched {
        cx.state.listeners.borrow_mut().remove_by_id(this, id);
    }
    Ok(())
}

pub(crate) fn dispatch_event(
    cx: &BindCx<'_>,
    this: EventTargetKey,
    event_value: JsValue,
) -> Result<bool, JsThrow> {
    let event = cx.this_event(&event_value)?;
    event.borrow_mut().is_trusted = false;
    events::dispatch_event(cx, this, &event_value, &event)
}
