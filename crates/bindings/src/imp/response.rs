//! `Response` implementation. The body is fully buffered (Phase 3 fetch does
//! not stream), so `text()`/`json()`/`arrayBuffer()` resolve immediately.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::filedata::{BlobData, normalize_type};
use crate::netdata::{HeadersData, ResponseData};
use crate::state::HostData;

type Resp = Rc<ResponseData>;

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    body: JsValue,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let (status, status_text, header_pairs) = match &init {
        JsValue::Object(obj) => {
            let status = cx
                .scope
                .get(obj, "status")
                .ok()
                .and_then(|v| v.as_number())
                .map(|n| n as u16)
                .unwrap_or(200);
            let status_text = cx
                .scope
                .get(obj, "statusText")
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            let headers = cx.scope.get(obj, "headers").unwrap_or(JsValue::Undefined);
            let pairs = if headers.is_nullish() {
                Vec::new()
            } else if let Ok(h) = cx.this_headers(&headers) {
                h.borrow().entries.clone()
            } else {
                cx.entries_of(&headers)?
            };
            (status, status_text, pairs)
        }
        _ => (200, String::new(), Vec::new()),
    };
    let body_bytes = match &body {
        JsValue::Undefined | JsValue::Null => Vec::new(),
        other => cx
            .scope
            .coerce_string(other)
            .map_err(JsThrow::from)?
            .into_bytes(),
    };
    let data = ResponseData {
        status,
        status_text,
        url: String::new(),
        redirected: false,
        resp_type: "default".to_owned(),
        headers: Rc::new(RefCell::new(HeadersData::from_pairs(&header_pairs))),
        body: body_bytes,
        body_used: Cell::new(false),
    };
    cx.new_net_object("Response", HostData::Response(Rc::new(data)))
}

pub(crate) fn r#type(_cx: &BindCx<'_>, this: Resp) -> Result<String, JsThrow> {
    Ok(this.resp_type.clone())
}

pub(crate) fn url(_cx: &BindCx<'_>, this: Resp) -> Result<String, JsThrow> {
    Ok(this.url.clone())
}

pub(crate) fn redirected(_cx: &BindCx<'_>, this: Resp) -> Result<bool, JsThrow> {
    Ok(this.redirected)
}

pub(crate) fn status(_cx: &BindCx<'_>, this: Resp) -> Result<f64, JsThrow> {
    Ok(f64::from(this.status))
}

pub(crate) fn ok(_cx: &BindCx<'_>, this: Resp) -> Result<bool, JsThrow> {
    Ok((200..=299).contains(&this.status))
}

pub(crate) fn status_text(_cx: &BindCx<'_>, this: Resp) -> Result<String, JsThrow> {
    Ok(this.status_text.clone())
}

pub(crate) fn headers(cx: &BindCx<'_>, this: Resp) -> Result<JsValue, JsThrow> {
    cx.new_net_object("Headers", HostData::Headers(Rc::clone(&this.headers)))
}

pub(crate) fn body_used(_cx: &BindCx<'_>, this: Resp) -> Result<bool, JsThrow> {
    Ok(this.body_used.get())
}

/// Rejects (with a `TypeError`) when the body has already been consumed, per
/// the Fetch "consume body" algorithm; otherwise marks it consumed.
fn take_body(cx: &BindCx<'_>, this: &Resp) -> Result<(), JsValue> {
    if this.body_used.get() {
        return Err(cx.type_error_value("Response body is already used"));
    }
    this.body_used.set(true);
    Ok(())
}

pub(crate) fn text(cx: &BindCx<'_>, this: Resp) -> Result<JsValue, JsThrow> {
    if let Err(err) = take_body(cx, &this) {
        return cx.rejected_promise(err);
    }
    let text = String::from_utf8_lossy(&this.body).into_owned();
    cx.resolved_promise(JsValue::String(text))
}

pub(crate) fn json(cx: &BindCx<'_>, this: Resp) -> Result<JsValue, JsThrow> {
    if let Err(err) = take_body(cx, &this) {
        return cx.rejected_promise(err);
    }
    let text = String::from_utf8_lossy(&this.body).into_owned();
    // `Promise.resolve(text).then(JSON.parse)` so malformed JSON rejects the
    // returned promise rather than throwing synchronously.
    let promise = cx.resolved_promise(JsValue::String(text))?;
    let JsValue::Object(promise_obj) = &promise else {
        return Ok(promise);
    };
    let then = cx.scope.get(promise_obj, "then").map_err(JsThrow::from)?;
    let parse = json_parse(cx)?;
    cx.scope
        .call(&then, &promise, &[parse])
        .map_err(JsThrow::from)
}

pub(crate) fn array_buffer(cx: &BindCx<'_>, this: Resp) -> Result<JsValue, JsThrow> {
    if let Err(err) = take_body(cx, &this) {
        return cx.rejected_promise(err);
    }
    let buffer = cx.bytes_to_array_buffer(&this.body)?;
    cx.resolved_promise(buffer)
}

/// `blob()`: the body as a `Blob` whose `type` is the response's own
/// `Content-Type`, per Fetch's "package data" — the header, not a guess, which
/// is what makes `URL.createObjectURL`-free consumers (an `<img>` fed through
/// `FileReader`) see the right MIME type.
pub(crate) fn blob(cx: &BindCx<'_>, this: Resp) -> Result<JsValue, JsThrow> {
    if let Err(err) = take_body(cx, &this) {
        return cx.rejected_promise(err);
    }
    let content_type = this
        .headers
        .borrow()
        .get("content-type")
        .unwrap_or_default();
    let data = BlobData::new(this.body.clone(), normalize_type(&content_type));
    let blob = cx.new_blob(Rc::new(data))?;
    cx.resolved_promise(blob)
}

/// The global `JSON.parse` function.
fn json_parse(cx: &BindCx<'_>) -> Result<JsValue, JsThrow> {
    let global = cx.with_global()?;
    let json = cx.scope.get(&global, "JSON").map_err(JsThrow::from)?;
    let JsValue::Object(json) = &json else {
        return Err(JsThrow::Type("JSON is not available".into()));
    };
    cx.scope.get(json, "parse").map_err(JsThrow::from)
}
