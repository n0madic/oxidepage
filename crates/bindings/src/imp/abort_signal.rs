//! `AbortSignal` members and the shared signal-abort algorithm.

use std::rc::Rc;

use oxidepage_base::RequestId;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{self, EventData, EventTargetKey};
use crate::netdata::PendingNet;
use crate::state::AbortSignalData;

fn target(this: &AbortSignalData) -> Result<EventTargetKey, JsThrow> {
    this.key
        .get()
        .map(EventTargetKey::AbortSignal)
        .ok_or_else(|| JsThrow::Type("AbortSignal is not installed".into()))
}

pub(crate) fn aborted(_cx: &BindCx<'_>, this: Rc<AbortSignalData>) -> Result<bool, JsThrow> {
    Ok(this.aborted.get())
}

pub(crate) fn reason(_cx: &BindCx<'_>, this: Rc<AbortSignalData>) -> Result<JsValue, JsThrow> {
    Ok(this.reason.borrow().clone())
}

pub(crate) fn throw_if_aborted(_cx: &BindCx<'_>, this: Rc<AbortSignalData>) -> Result<(), JsThrow> {
    if this.aborted.get() {
        return Err(JsThrow::Value(this.reason.borrow().clone()));
    }
    Ok(())
}

pub(crate) fn onabort(cx: &BindCx<'_>, this: Rc<AbortSignalData>) -> Result<JsValue, JsThrow> {
    Ok(cx
        .state
        .event_handlers
        .borrow()
        .get(&(target(&this)?, "abort".to_owned()))
        .cloned()
        .unwrap_or(JsValue::Null))
}

pub(crate) fn set_onabort(
    cx: &BindCx<'_>,
    this: Rc<AbortSignalData>,
    value: JsValue,
) -> Result<(), JsThrow> {
    let key = (target(&this)?, "abort".to_owned());
    if cx.scope.is_function(&value) {
        cx.state.event_handlers.borrow_mut().insert(key, value);
    } else {
        cx.state.event_handlers.borrow_mut().remove(&key);
    }
    Ok(())
}

/// The signal-abort algorithm shared by `AbortController.abort()` and
/// `AbortSignal.abort()`/`timeout()`.
///
/// Order (matching browsers): set `aborted`/`reason`, cancel the fetches tied
/// to this signal (`hooks.abort` + remove from `pending_net` + reject their
/// promises — which only queues a promise job), then fire `abort` (running
/// `onabort` and listeners synchronously). A second call is a no-op.
///
/// The `abort` event is dispatched *without* a trailing microtask checkpoint:
/// the reject reactions (`.catch`) and any other queued microtasks run at the
/// current task's natural checkpoint, after `abort()` returns — draining them
/// inside `abort()` would reorder unrelated page microtasks (the exact hazard
/// `fire_simple_event` warns about for mid-task dispatch).
pub(crate) fn signal_abort(
    cx: &BindCx<'_>,
    data: &Rc<AbortSignalData>,
    reason: JsValue,
) -> Result<(), JsThrow> {
    if data.aborted.get() {
        return Ok(());
    }
    // WebIDL: an undefined reason becomes a fresh "AbortError" DOMException; an
    // explicit `null` is preserved.
    let reason = if reason.is_undefined() {
        cx.make_dom_exception_value("AbortError", "signal is aborted without reason")?
    } else {
        reason
    };
    data.aborted.set(true);
    *data.reason.borrow_mut() = reason.clone();

    let ids: Vec<RequestId> = std::mem::take(&mut *data.pending_fetches.borrow_mut());
    for id in ids {
        cx.state.hooks.abort(id);
        // Remove the entry (dropping the borrow) before rejecting, so the
        // reject reaction cannot observe a half-updated `pending_net`. Missing
        // ids (already completed) are lazily pruned by the take above.
        let entry = cx.state.pending_net.borrow_mut().remove(&id);
        if let Some(PendingNet::Fetch { reject, .. }) = entry {
            let _ = cx
                .scope
                .call(&reject, &JsValue::Undefined, std::slice::from_ref(&reason));
        }
    }

    let key = target(data)?;

    // Remove every listener that was added with `{ signal }` — *before* the abort
    // event is dispatched. A listener aborting mid-dispatch must not see the
    // listeners it just removed still run (WPT: "Aborting from a listener does
    // not call future listeners"), and dispatch snapshots the list it iterates.
    if let Some(signal_key) = data.key.get() {
        cx.state.listeners.borrow_mut().remove_by_signal(signal_key);
    }

    let mut ev_data = EventData::new("abort".to_owned(), false, false, false);
    ev_data.is_trusted = true;
    let (value, event) = cx.new_event_object("Event", ev_data)?;
    events::dispatch_event(cx, key, &value, &event)?;
    Ok(())
}
