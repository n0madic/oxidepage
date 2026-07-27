//! `Request` implementation. Like `Response`, the body is fully buffered
//! (Phase 3 fetch does not stream), so `text()`/`json()`/`arrayBuffer()`
//! resolve immediately. Enum-valued members are stored as their serialized
//! strings; `fetch` maps `mode`/`credentials` back onto the net enums.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxidepage_js::{HostCall, JsObject, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::netdata::{HeadersData, RequestData};
use crate::state::HostData;

type Req = Rc<RequestData>;

/// Reads a string-valued member of the `RequestInit` dictionary, treating a
/// nullish or non-string value as "absent".
fn init_str(cx: &BindCx<'_>, obj: &JsObject, key: &str) -> Option<String> {
    cx.scope
        .get(obj, key)
        .ok()
        .filter(|v| !v.is_nullish())
        .and_then(|v| v.as_str().map(str::to_owned))
}

/// Resolves `raw` against the document base URL, mirroring `fetch`.
fn resolve_url(cx: &BindCx<'_>, raw: &str) -> Result<String, JsThrow> {
    match url::Url::parse(raw) {
        Ok(u) => Ok(u.to_string()),
        Err(_) => {
            let doc_url = cx.state.dom.borrow().document_url().to_owned();
            url::Url::parse(&doc_url)
                .and_then(|base| base.join(raw))
                .map(|u| u.to_string())
                .map_err(|_| JsThrow::Type(format!("Request: invalid URL `{raw}`")))
        }
    }
}

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    input: JsValue,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    // Seed from `input`: an existing Request (copy its state) or a URL string.
    let (
        mut method,
        url,
        mut header_pairs,
        mut body,
        mut referrer,
        mut referrer_policy,
        mut mode,
        mut credentials,
        mut cache,
        mut redirect,
        mut integrity,
        mut keepalive,
        destination,
    ) = if let Ok(req) = cx.this_request(&input) {
        (
            req.method.clone(),
            req.url.clone(),
            req.headers.borrow().entries.clone(),
            req.body.clone(),
            req.referrer.clone(),
            req.referrer_policy.clone(),
            req.mode.clone(),
            req.credentials.clone(),
            req.cache.clone(),
            req.redirect.clone(),
            req.integrity.clone(),
            req.keepalive,
            req.destination.clone(),
        )
    } else {
        let raw = cx.scope.coerce_string(&input).map_err(JsThrow::from)?;
        (
            "GET".to_owned(),
            resolve_url(cx, &raw)?,
            Vec::new(),
            None,
            "about:client".to_owned(),
            String::new(),
            "cors".to_owned(),
            "same-origin".to_owned(),
            "default".to_owned(),
            "follow".to_owned(),
            String::new(),
            false,
            String::new(),
        )
    };

    // Apply `init` overrides.
    if let JsValue::Object(obj) = &init {
        if let Some(m) = init_str(cx, obj, "method") {
            method = m.to_ascii_uppercase();
        }
        if let Some(v) = init_str(cx, obj, "mode") {
            mode = v;
        }
        if let Some(v) = init_str(cx, obj, "credentials") {
            credentials = v;
        }
        if let Some(v) = init_str(cx, obj, "cache") {
            cache = v;
        }
        if let Some(v) = init_str(cx, obj, "redirect") {
            redirect = v;
        }
        if let Some(v) = init_str(cx, obj, "referrer") {
            referrer = v;
        }
        if let Some(v) = init_str(cx, obj, "referrerPolicy") {
            referrer_policy = v;
        }
        if let Some(v) = init_str(cx, obj, "integrity") {
            integrity = v;
        }
        if let Ok(JsValue::Bool(k)) = cx.scope.get(obj, "keepalive") {
            keepalive = k;
        }

        let headers = cx.scope.get(obj, "headers").unwrap_or(JsValue::Undefined);
        if !headers.is_nullish() {
            header_pairs = if let Ok(hd) = cx.this_headers(&headers) {
                hd.borrow().entries.clone()
            } else {
                // A plain object or record: validate through the same path the
                // `Headers` constructor uses, so an invalid name/value is a
                // synchronous `TypeError` (per Fetch).
                let mut data = HeadersData::default();
                for (name, value) in cx.entries_of(&headers)? {
                    data.append(&name, &value)?;
                }
                data.entries
            };
        }

        let b = cx.scope.get(obj, "body").unwrap_or(JsValue::Undefined);
        if !b.is_nullish() {
            if method == "GET" || method == "HEAD" {
                return Err(JsThrow::Type(
                    "Request with GET/HEAD method cannot have body".into(),
                ));
            }
            body = Some(
                cx.scope
                    .coerce_string(&b)
                    .map_err(JsThrow::from)?
                    .into_bytes(),
            );
        }
    }

    let data = RequestData {
        method,
        url,
        headers: Rc::new(RefCell::new(HeadersData::from_pairs(&header_pairs))),
        destination,
        referrer,
        referrer_policy,
        mode,
        credentials,
        cache,
        redirect,
        integrity,
        keepalive,
        body,
        body_used: Cell::new(false),
    };
    cx.new_net_object("Request", HostData::Request(Rc::new(data)))
}

pub(crate) fn method(_cx: &BindCx<'_>, this: Req) -> Result<String, JsThrow> {
    Ok(this.method.clone())
}

pub(crate) fn url(_cx: &BindCx<'_>, this: Req) -> Result<String, JsThrow> {
    Ok(this.url.clone())
}

pub(crate) fn headers(cx: &BindCx<'_>, this: Req) -> Result<JsValue, JsThrow> {
    cx.new_net_object("Headers", HostData::Headers(Rc::clone(&this.headers)))
}

pub(crate) fn destination(_cx: &BindCx<'_>, this: Req) -> Result<String, JsThrow> {
    Ok(this.destination.clone())
}

pub(crate) fn referrer(_cx: &BindCx<'_>, this: Req) -> Result<String, JsThrow> {
    Ok(this.referrer.clone())
}

pub(crate) fn referrer_policy(_cx: &BindCx<'_>, this: Req) -> Result<String, JsThrow> {
    Ok(this.referrer_policy.clone())
}

pub(crate) fn mode(_cx: &BindCx<'_>, this: Req) -> Result<String, JsThrow> {
    Ok(this.mode.clone())
}

pub(crate) fn credentials(_cx: &BindCx<'_>, this: Req) -> Result<String, JsThrow> {
    Ok(this.credentials.clone())
}

pub(crate) fn cache(_cx: &BindCx<'_>, this: Req) -> Result<String, JsThrow> {
    Ok(this.cache.clone())
}

pub(crate) fn redirect(_cx: &BindCx<'_>, this: Req) -> Result<String, JsThrow> {
    Ok(this.redirect.clone())
}

pub(crate) fn integrity(_cx: &BindCx<'_>, this: Req) -> Result<String, JsThrow> {
    Ok(this.integrity.clone())
}

pub(crate) fn keepalive(_cx: &BindCx<'_>, this: Req) -> Result<bool, JsThrow> {
    Ok(this.keepalive)
}

pub(crate) fn body_used(_cx: &BindCx<'_>, this: Req) -> Result<bool, JsThrow> {
    Ok(this.body_used.get())
}

/// Rejects (with a `TypeError`) when the body has already been consumed, per
/// the Fetch "consume body" algorithm; otherwise marks it consumed.
fn take_body(cx: &BindCx<'_>, this: &Req) -> Result<(), JsValue> {
    if this.body_used.get() {
        return Err(cx.type_error_value("Request body is already used"));
    }
    this.body_used.set(true);
    Ok(())
}

fn body_text(this: &Req) -> String {
    this.body
        .as_deref()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
}

pub(crate) fn text(cx: &BindCx<'_>, this: Req) -> Result<JsValue, JsThrow> {
    if let Err(err) = take_body(cx, &this) {
        return cx.rejected_promise(err);
    }
    cx.resolved_promise(JsValue::String(body_text(&this)))
}

pub(crate) fn json(cx: &BindCx<'_>, this: Req) -> Result<JsValue, JsThrow> {
    if let Err(err) = take_body(cx, &this) {
        return cx.rejected_promise(err);
    }
    // `Promise.resolve(text).then(JSON.parse)` so malformed JSON rejects the
    // returned promise rather than throwing synchronously.
    let promise = cx.resolved_promise(JsValue::String(body_text(&this)))?;
    let JsValue::Object(promise_obj) = &promise else {
        return Ok(promise);
    };
    let then = cx.scope.get(promise_obj, "then").map_err(JsThrow::from)?;
    let parse = json_parse(cx)?;
    cx.scope
        .call(&then, &promise, &[parse])
        .map_err(JsThrow::from)
}

pub(crate) fn array_buffer(cx: &BindCx<'_>, this: Req) -> Result<JsValue, JsThrow> {
    if let Err(err) = take_body(cx, &this) {
        return cx.rejected_promise(err);
    }
    let buffer = cx.bytes_to_array_buffer(this.body.as_deref().unwrap_or_default())?;
    cx.resolved_promise(buffer)
}

pub(crate) fn clone(cx: &BindCx<'_>, this: Req) -> Result<JsValue, JsThrow> {
    if this.body_used.get() {
        return Err(JsThrow::Type(
            "Cannot clone a Request whose body is already used".into(),
        ));
    }
    let data = RequestData {
        method: this.method.clone(),
        url: this.url.clone(),
        headers: Rc::new(RefCell::new(HeadersData {
            entries: this.headers.borrow().entries.clone(),
        })),
        destination: this.destination.clone(),
        referrer: this.referrer.clone(),
        referrer_policy: this.referrer_policy.clone(),
        mode: this.mode.clone(),
        credentials: this.credentials.clone(),
        cache: this.cache.clone(),
        redirect: this.redirect.clone(),
        integrity: this.integrity.clone(),
        keepalive: this.keepalive,
        body: this.body.clone(),
        body_used: Cell::new(false),
    };
    cx.new_net_object("Request", HostData::Request(Rc::new(data)))
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
