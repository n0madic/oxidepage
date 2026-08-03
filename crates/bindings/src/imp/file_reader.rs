//! `FileReader` implementation (File API).
//!
//! A real `EventTarget`, for the reason `XMLHttpRequest` is one: its slab key
//! *is* its [`EventTargetKey::Host`] identity, so `addEventListener` and the
//! `onX` properties are the same registry, and the events it fires are genuine
//! `ProgressEvent` objects.
//!
//! **A read completes in a task, never inline** (ADR-0032 D10). The bytes are
//! already in memory, so resolving synchronously would be trivial — and wrong:
//! `reader.readAsText(b); reader.onload = …` must still see `load`, and code
//! that sequences work off `onloadend` would run it before the caller's own
//! statement after `readAsText` had. The read is therefore two queued tasks —
//! one firing `loadstart`, one completing — which is also what gives `abort()`
//! a window to land in.
//!
//! Deliberately absent (ADR-0032's limits): `readAsBinaryString`, and progress
//! events beyond `loadstart`/`load`/`loadend` — there is no partial delivery to
//! report progress against, and a fabricated 50% event would be a lie.

use std::rc::Rc;

use oxidepage_base::DomExceptionKind;
use oxidepage_js::{HostCall, HostFn, JsThrow, JsValue};
use oxidepage_net::{charset_from_content_type, decode_with_charset};

use crate::cx::BindCx;
use crate::events::EventTargetKey;
use crate::filedata::{BlobData, DONE, EMPTY, FileReaderData, LOADING, ReadKind};

type Reader = Rc<FileReaderData>;

pub(crate) fn constructor(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.new_file_reader()
}

// === Read entry points ===

pub(crate) fn read_as_text(
    cx: &BindCx<'_>,
    this: Reader,
    blob: JsValue,
    encoding: Option<String>,
) -> Result<(), JsThrow> {
    start(cx, &this, &blob, ReadKind::Text, encoding)
}

pub(crate) fn read_as_data_url(
    cx: &BindCx<'_>,
    this: Reader,
    blob: JsValue,
) -> Result<(), JsThrow> {
    start(cx, &this, &blob, ReadKind::DataUrl, None)
}

pub(crate) fn read_as_array_buffer(
    cx: &BindCx<'_>,
    this: Reader,
    blob: JsValue,
) -> Result<(), JsThrow> {
    start(cx, &this, &blob, ReadKind::ArrayBuffer, None)
}

/// The spec's shared "read operation": brand-check the blob, refuse a
/// concurrent read, reset the result, then queue the two tasks.
fn start(
    cx: &BindCx<'_>,
    this: &Reader,
    blob: &JsValue,
    kind: ReadKind,
    encoding: Option<String>,
) -> Result<(), JsThrow> {
    let blob = cx.this_blob(blob)?;
    if this.ready_state.get() == LOADING {
        return Err(cx.dom_throw(
            DomExceptionKind::InvalidStateError,
            "FileReader: a read is already in progress",
        ));
    }
    this.ready_state.set(LOADING);
    *this.result.borrow_mut() = JsValue::Null;
    *this.error.borrow_mut() = JsValue::Null;
    let token = this.token.get().wrapping_add(1);
    this.token.set(token);

    let reader = Rc::clone(this);
    queue_task(cx, "FileReader loadstart", move |cx| {
        if !still_reading(&reader, token) {
            return;
        }
        fire(cx, &reader, "loadstart", 0.0, blob.size() as f64);
        // Queued from *inside* the first task rather than alongside it: two
        // timers armed at the same instant are only ordered if the embedder's
        // heap is stable, and this makes the ordering structural instead.
        let reader = Rc::clone(&reader);
        let blob = Rc::clone(&blob);
        let encoding = encoding.clone();
        let queued = queue_task(cx, "FileReader complete", move |cx| {
            complete(cx, &reader, token, &blob, kind, encoding.as_deref());
        });
        if let Err(e) = queued {
            cx.warn(&format!("FileReader: could not queue completion: {e:?}"));
        }
    })
}

/// True while the read that issued `token` is still the reader's current one —
/// `abort()` and a second `readAs*` both bump it, and neither can recall a task
/// already handed to the embedder.
fn still_reading(this: &Reader, token: u64) -> bool {
    this.ready_state.get() == LOADING && this.token.get() == token
}

/// The completion task: compute the result, go to DONE, fire `load` then
/// `loadend`.
fn complete(
    cx: &BindCx<'_>,
    this: &Reader,
    token: u64,
    blob: &Rc<BlobData>,
    kind: ReadKind,
    encoding: Option<&str>,
) {
    if !still_reading(this, token) {
        return;
    }
    let size = blob.size() as f64;
    let result = match build_result(cx, blob, kind, encoding) {
        Ok(value) => value,
        // A failed read fires `error`, **not** `load` with a null result. The
        // spec says so, and P6 requires it: `onerror` is an installable handler,
        // so one that can never fire is the always-failing stub this project
        // does not ship. `NotReadableError` is the spec's own failure reason.
        Err(e) => {
            cx.warn(&format!("FileReader: could not build the result: {e:?}"));
            this.ready_state.set(DONE);
            *this.result.borrow_mut() = JsValue::Null;
            // The exception as a *value*, not a throw: `reader.error` is a
            // property a handler reads, and nothing here is throwing.
            *this.error.borrow_mut() = match cx.dom_throw(
                DomExceptionKind::NotReadableError,
                "the blob could not be read",
            ) {
                JsThrow::Value(value) => value,
                _ => JsValue::Null,
            };
            fire(cx, this, "error", 0.0, size);
            fire(cx, this, "loadend", 0.0, size);
            return;
        }
    };
    this.ready_state.set(DONE);
    *this.result.borrow_mut() = result;
    fire(cx, this, "load", size, size);
    fire(cx, this, "loadend", size, size);
}

fn build_result(
    cx: &BindCx<'_>,
    blob: &Rc<BlobData>,
    kind: ReadKind,
    encoding: Option<&str>,
) -> Result<JsValue, JsThrow> {
    Ok(match kind {
        ReadKind::Text => {
            // The spec's encoding determination: the explicit argument wins,
            // then a `charset=` in the blob's own type, then UTF-8.
            let label = encoding
                .filter(|label| !label.is_empty())
                .or_else(|| charset_from_content_type(&blob.type_))
                .unwrap_or("utf-8");
            JsValue::String(decode_with_charset(blob.view(), label))
        }
        ReadKind::DataUrl => JsValue::String(format!(
            "data:{};base64,{}",
            blob.type_,
            base64_encode(blob.view())
        )),
        ReadKind::ArrayBuffer => cx.bytes_to_array_buffer(blob.view())?,
    })
}

pub(crate) fn abort(cx: &BindCx<'_>, this: Reader) -> Result<(), JsThrow> {
    // EMPTY or DONE: the spec sets the result to null and returns — an abort
    // of nothing fires nothing.
    if matches!(this.ready_state.get(), EMPTY | DONE) {
        *this.result.borrow_mut() = JsValue::Null;
        return Ok(());
    }
    this.ready_state.set(DONE);
    *this.result.borrow_mut() = JsValue::Null;
    // Invalidates the queued completion task; there is no partial result to
    // report, which ADR-0032 records as a deliberate limit.
    this.token.set(this.token.get().wrapping_add(1));
    fire(cx, &this, "abort", 0.0, 0.0);
    fire(cx, &this, "loadend", 0.0, 0.0);
    Ok(())
}

// === Accessors ===

pub(crate) fn ready_state(_cx: &BindCx<'_>, this: Reader) -> Result<f64, JsThrow> {
    Ok(f64::from(this.ready_state.get()))
}

pub(crate) fn result(_cx: &BindCx<'_>, this: Reader) -> Result<JsValue, JsThrow> {
    Ok(this.result.borrow().clone())
}

pub(crate) fn error(_cx: &BindCx<'_>, this: Reader) -> Result<JsValue, JsThrow> {
    Ok(this.error.borrow().clone())
}

/// The six handler properties, over the shared `event_handlers` registry —
/// the same storage `addEventListener` uses, which is what puts a handler and
/// a listener on equal footing (see `imp::xhr_event_target`).
macro_rules! handler {
    ($getter:ident, $setter:ident, $event_type:literal) => {
        pub(crate) fn $getter(cx: &BindCx<'_>, this: Reader) -> Result<JsValue, JsThrow> {
            Ok(super::xhr_event_target::get(cx, key(&this), $event_type))
        }
        pub(crate) fn $setter(
            cx: &BindCx<'_>,
            this: Reader,
            value: JsValue,
        ) -> Result<(), JsThrow> {
            super::xhr_event_target::set(cx, key(&this), $event_type, value);
            Ok(())
        }
    };
}

handler!(onloadstart, set_onloadstart, "loadstart");
handler!(onprogress, set_onprogress, "progress");
handler!(onload, set_onload, "load");
handler!(onabort, set_onabort, "abort");
handler!(onerror, set_onerror, "error");
handler!(onloadend, set_onloadend, "loadend");

// === Plumbing ===

fn key(this: &Reader) -> EventTargetKey {
    EventTargetKey::Host(this.slab_key.get())
}

/// Fires one `ProgressEvent` through the real dispatch machinery.
fn fire(cx: &BindCx<'_>, this: &Reader, event_type: &str, loaded: f64, total: f64) {
    let mut data = super::progress_event::event_data(event_type, true, loaded, total);
    data.is_trusted = true;
    data.time_stamp = cx.now_ms();
    let Ok((value, data)) = cx.new_event_object("ProgressEvent", data) else {
        return;
    };
    if let Err(e) = crate::events::dispatch_event(cx, key(this), &value, &data) {
        cx.warn(&format!("FileReader `{event_type}` dispatch failed: {e:?}"));
    }
}

/// Queues `f` as a task on the embedder's event loop.
///
/// A zero-delay timer, because that *is* the task-queuing primitive the
/// bindings have: [`crate::state::HostHooks`] exposes one queue, and inventing
/// a second one for this would put `FileReader` completions in an order the
/// page's loop does not define against timers.
fn queue_task(
    cx: &BindCx<'_>,
    name: &str,
    f: impl Fn(&BindCx<'_>) + 'static,
) -> Result<(), JsThrow> {
    let host: HostFn = Rc::new(move |scope, _call| {
        let cx = BindCx {
            scope,
            state: crate::cx::page_state(scope)?,
        };
        f(&cx);
        Ok(JsValue::Undefined)
    });
    let func = cx
        .scope
        .new_function(name, 0, host)
        .map_err(JsThrow::from)?;
    cx.state
        .hooks
        .schedule_timer(JsValue::Object(func), Vec::new(), 0.0, false);
    Ok(())
}

/// Standard base64 (RFC 4648), for `readAsDataURL`.
///
/// Written here rather than pulled in as a dependency: `crates/net` has the
/// forgiving-base64 *decoder* the `data:` URL processor needs, and this is the
/// one place in the tree that needs to go the other way.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    /// RFC 4648 §10's own vectors, which is what pins the padding cases: a
    /// 1-byte tail is `==` and a 2-byte tail is `=`.
    #[test]
    fn base64_matches_rfc_4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // The two high-bit alphabet entries only appear for bytes no ASCII
        // vector reaches.
        assert_eq!(base64_encode(&[0xff, 0xef, 0xbe]), "/+++");
    }

    /// `EMPTY`/`LOADING`/`DONE` must agree with the IDL constants, which are
    /// emitted from `file_api.webidl` and never from this module.
    #[test]
    fn ready_states_match_the_idl_constants() {
        assert_eq!((super::EMPTY, super::LOADING, super::DONE), (0, 1, 2));
    }
}
