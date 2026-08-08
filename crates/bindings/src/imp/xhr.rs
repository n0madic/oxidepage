//! `XMLHttpRequest` implementation.
//!
//! `XMLHttpRequest` is a real `EventTarget`: its listeners and `onX` handlers
//! live in the shared registries keyed by
//! [`crate::events::EventTargetKey::Host`], and its events go through
//! [`crate::events::dispatch_event`] like any other. The slab key doubles as
//! the event-target identity, the same scheme `new EventTarget()` uses.
//!
//! The state machine models the spec's flags explicitly (send() flag, upload
//! complete flag, response object cache) — see [`crate::netdata::XhrData`] and
//! ADR-0024, which also records the deliberate absences: no synchronous mode, a
//! single-shot `progress` event (the net layer buffers the whole body), and a
//! page-side rather than per-request `timeout`. The fourth, `responseType =
//! "blob"`, is no longer one: ADR-0032 Phase 4 brought the `Blob` interface it
//! was waiting on.

use std::cell::RefCell;
use std::rc::Rc;

use oxidepage_base::{DomExceptionKind, NodeId};
use oxidepage_js::{HostCall, HostFn, JsThrow, JsValue};
use oxidepage_net::{
    Credentials, NetRequest, RequestMode, ResourceType, charset_from_content_type,
    decode_with_charset, is_forbidden_request_header,
};

use crate::cx::BindCx;
use crate::events::{EventData, EventTargetKey};
use crate::filedata::{BlobData, normalize_type};
use crate::netdata::{PendingNet, XhrData, XhrRef, is_valid_header_name, is_valid_header_value};
use crate::state::HostData;

pub(crate) const UNSENT: u16 = 0;
pub(crate) const OPENED: u16 = 1;
pub(crate) const HEADERS_RECEIVED: u16 = 2;
pub(crate) const LOADING: u16 = 3;
pub(crate) const DONE: u16 = 4;

/// The `responseType` enumeration, complete as of ADR-0032 Phase 4 — `"blob"`
/// was the one absence, and it was absent because the engine had no `Blob`
/// type at all (ADR-0024, P6: an enumerated attribute ignores a value outside
/// its set, so assigning an unsupported one left the previous value rather
/// than installing a mode that could only ever return null).
const RESPONSE_TYPES: &[&str] = &["", "arraybuffer", "blob", "document", "json", "text"];

/// Methods `open()` must reject outright (Fetch's "forbidden method").
const FORBIDDEN_METHODS: &[&str] = &["CONNECT", "TRACE", "TRACK"];

pub(crate) fn constructor(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    let xhr = Rc::new(RefCell::new(XhrData::default()));
    let wrapper = cx.new_net_object("XMLHttpRequest", HostData::Xhr(Rc::clone(&xhr)))?;
    // The slab key is this object's event-target identity for its whole life;
    // every listener and handler is filed under it. Failing loudly is the only
    // safe answer: key 0 is a *valid* `EventTargetKey::Host`, so a silent
    // `unwrap_or_default` would file two XHRs (and an XHR and its own upload
    // object, whose key defaults the same way) under one identity and
    // cross-deliver their `load`/`readystatechange` events.
    let slab_key = cx.slab_key(&wrapper).ok_or_else(|| {
        JsThrow::Type("XMLHttpRequest: the host object has no event-target identity".into())
    })?;
    {
        let mut x = xhr.borrow_mut();
        x.slab_key = slab_key;
        x.wrapper = Some(wrapper.clone());
    }
    Ok(wrapper)
}

// === Simple state accessors ===

pub(crate) fn ready_state(_cx: &BindCx<'_>, this: XhrRef) -> Result<f64, JsThrow> {
    Ok(f64::from(this.borrow().ready_state))
}

pub(crate) fn status(_cx: &BindCx<'_>, this: XhrRef) -> Result<f64, JsThrow> {
    Ok(f64::from(this.borrow().status))
}

pub(crate) fn status_text(_cx: &BindCx<'_>, this: XhrRef) -> Result<String, JsThrow> {
    Ok(this.borrow().status_text.clone())
}

pub(crate) fn response_url(_cx: &BindCx<'_>, this: XhrRef) -> Result<String, JsThrow> {
    Ok(this.borrow().response_url.clone())
}

pub(crate) fn with_credentials(_cx: &BindCx<'_>, this: XhrRef) -> Result<bool, JsThrow> {
    Ok(this.borrow().with_credentials)
}

pub(crate) fn set_with_credentials(
    cx: &BindCx<'_>,
    this: XhrRef,
    value: bool,
) -> Result<(), JsThrow> {
    let x = this.borrow();
    if !matches!(x.ready_state, UNSENT | OPENED) || x.send_flag {
        return Err(cx.dom_throw(
            DomExceptionKind::InvalidStateError,
            "XMLHttpRequest.withCredentials: the request has already been sent",
        ));
    }
    drop(x);
    this.borrow_mut().with_credentials = value;
    Ok(())
}

pub(crate) fn timeout(_cx: &BindCx<'_>, this: XhrRef) -> Result<f64, JsThrow> {
    Ok(f64::from(this.borrow().timeout))
}

/// `timeout` is settable *during* a request, and then it re-arms against the
/// moment `send()` was called — the spec measures the timeout from there, not
/// from the assignment.
pub(crate) fn set_timeout(cx: &BindCx<'_>, this: XhrRef, value: u32) -> Result<(), JsThrow> {
    this.borrow_mut().timeout = value;
    if this.borrow().send_flag {
        arm_timeout(cx, &this)?;
    }
    Ok(())
}

/// The `[SameObject]` upload object, created on first access.
pub(crate) fn upload(cx: &BindCx<'_>, this: XhrRef) -> Result<JsValue, JsThrow> {
    if let Some(existing) = this.borrow().upload.clone() {
        return Ok(existing);
    }
    let wrapper = cx.new_xhr_upload(Rc::downgrade(&this.data))?;
    // As in `constructor`: key 0 is a real identity, so falling back to it
    // would alias the upload object with whatever else took that fallback.
    let key = cx.slab_key(&wrapper).ok_or_else(|| {
        JsThrow::Type("XMLHttpRequestUpload: the host object has no event-target identity".into())
    })?;
    let mut x = this.borrow_mut();
    x.upload_key = key;
    x.upload = Some(wrapper.clone());
    Ok(wrapper)
}

// === open / send / abort ===

pub(crate) fn open(
    cx: &BindCx<'_>,
    this: XhrRef,
    method: String,
    url: String,
    is_async: bool,
    _username: Option<String>,
    _password: Option<String>,
) -> Result<(), JsThrow> {
    if !is_valid_header_name(&method) {
        return Err(cx.dom_throw(
            DomExceptionKind::SyntaxError,
            "XMLHttpRequest.open: method is not a valid HTTP token",
        ));
    }
    let normalized = normalize_method(&method);
    if FORBIDDEN_METHODS.contains(&normalized.as_str()) {
        return Err(cx.dom_throw(
            DomExceptionKind::SecurityError,
            "XMLHttpRequest.open: forbidden method",
        ));
    }
    // No synchronous mode. A blocking net wait inside a JS call would run while
    // the `dom`/`style`/`layout` `RefCell`s are borrowed by the caller, so the
    // first thing the resumed page touched would panic. Absent and loud beats a
    // mode that deadlocks or corrupts (ADR-0024).
    if !is_async {
        return Err(cx.dom_throw(
            DomExceptionKind::InvalidAccessError,
            "XMLHttpRequest.open: synchronous mode is not supported",
        ));
    }
    let doc_url = cx.state.dom.borrow().document_url().to_owned();
    let absolute = match url::Url::parse(&url) {
        Ok(u) => u.to_string(),
        Err(_) => url::Url::parse(&doc_url)
            .and_then(|base| base.join(&url))
            .map(|u| u.to_string())
            .map_err(|_| {
                cx.dom_throw(
                    DomExceptionKind::SyntaxError,
                    &format!("XMLHttpRequest.open: invalid URL `{url}`"),
                )
            })?,
    };

    // "Terminate this's fetch controller": a reopened XHR must stop receiving
    // the old request's events, which otherwise keep writing into the very
    // state this call is resetting.
    terminate(cx, &this);

    let was_opened = {
        let mut x = this.borrow_mut();
        let was_opened = x.ready_state == OPENED;
        x.method = normalized;
        x.url = absolute;
        x.request_headers.clear();
        x.send_flag = false;
        x.upload_complete = false;
        x.set_network_error();
        x.ready_state = OPENED;
        // Re-root: every terminal transition released the self-reference, and
        // without putting it back a *reused* XHR would fire no events at all —
        // `fire_at` needs the wrapper for `event.target`.
        x.wrapper = Some(this.wrapper.clone());
        was_opened
    };
    // `open()` step 11 fires `readystatechange` only "if this's state is not
    // opened". `open(); open()` on a fresh object must produce one transition,
    // not two — code that drives a state machine off `onreadystatechange`
    // counts them.
    if !was_opened {
        fire_plain(cx, &this, "readystatechange");
    }
    Ok(())
}

pub(crate) fn set_request_header(
    cx: &BindCx<'_>,
    this: XhrRef,
    name: String,
    value: String,
) -> Result<(), JsThrow> {
    {
        let x = this.borrow();
        if x.ready_state != OPENED || x.send_flag {
            return Err(cx.dom_throw(
                DomExceptionKind::InvalidStateError,
                "XMLHttpRequest.setRequestHeader: the request is not open",
            ));
        }
    }
    // Normalize the value (strip surrounding HTTP whitespace) then reject an
    // invalid name/value — a CR/LF/NUL here would inject request headers.
    let value = value.trim();
    if !is_valid_header_name(&name) || !is_valid_header_value(value) {
        return Err(cx.dom_throw(
            DomExceptionKind::SyntaxError,
            "XMLHttpRequest.setRequestHeader: invalid header name or value",
        ));
    }
    // A forbidden header name is *silently ignored*, not an error: the spec is
    // explicit, and feature-detecting code sets `User-Agent` and carries on.
    if is_forbidden_request_header(&name) {
        return Ok(());
    }
    let mut x = this.borrow_mut();
    // Setting the same header twice **combines**, it does not duplicate.
    if let Some((_, existing)) = x
        .request_headers
        .iter_mut()
        .find(|(n, _)| n.eq_ignore_ascii_case(&name))
    {
        existing.push_str(", ");
        existing.push_str(value);
    } else {
        x.request_headers.push((name, value.to_owned()));
    }
    Ok(())
}

pub(crate) fn send(cx: &BindCx<'_>, this: XhrRef, body: JsValue) -> Result<(), JsThrow> {
    {
        let x = this.borrow();
        if x.ready_state != OPENED || x.send_flag {
            return Err(cx.dom_throw(
                DomExceptionKind::InvalidStateError,
                "XMLHttpRequest.send: the request is not open",
            ));
        }
    }
    let (method, mut headers, url, with_credentials) = {
        let x = this.borrow();
        (
            x.method.clone(),
            x.request_headers.clone(),
            x.url.clone(),
            x.with_credentials,
        )
    };
    // A body is ignored for GET/HEAD, exactly as the spec says.
    let body_value = if matches!(method.as_str(), "GET" | "HEAD") {
        JsValue::Undefined
    } else {
        body
    };
    let body_bytes = match crate::imp::body::extract(cx, &body_value)? {
        None => None,
        Some(extracted) => {
            // The body's default `Content-Type` applies only if the author did
            // not set one with `setRequestHeader`. For a `FormData` body the
            // header carries the multipart boundary, so it cannot come from
            // anywhere else — which is exactly why jQuery sets
            // `contentType: false` when it sees one.
            if let Some(content_type) = extracted.content_type
                && !headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            {
                headers.push(("content-type".to_owned(), content_type));
            }
            Some(extracted.bytes)
        }
    };
    let body_len = body_bytes.as_ref().map_or(0.0, |b| b.len() as f64);

    let doc_url = cx.state.dom.borrow().document_url().to_owned();
    let initiator_origin = url::Url::parse(&doc_url)
        .ok()
        .map(|u| u.origin().ascii_serialization());

    // XHR credentials mode: `same-origin` unless `withCredentials` opts into
    // sending cookies (and credentialed CORS) on cross-origin requests.
    let credentials = if with_credentials {
        Credentials::Include
    } else {
        Credentials::SameOrigin
    };
    let request = NetRequest {
        // Script-initiated: no driver override.
        header_overrides: None,
        method,
        url,
        headers,
        body: body_bytes,
        credentials,
        mode: RequestMode::Cors,
        referrer: Some(doc_url),
        initiator_origin,
        bypass_cache: false,
        resource_type: ResourceType::Xhr,
        // Script never sets this: `Authorization` is a forbidden request header
        // and `open(url, user, password)`'s credentials are not implemented.
        auth: None,
    };
    let id = cx.state.hooks.start_fetch(request);
    // Tag the request with this world, so its completion is delivered here
    // and not to whichever world happens to be current (ADR-0033 D10).
    cx.state.frame.global.note_net_world(id, cx.state.id);
    {
        let mut x = this.borrow_mut();
        x.request_id = Some(id);
        x.send_flag = true;
        x.upload_complete = body_len == 0.0;
        x.upload_total = body_len;
        x.loaded = 0.0;
        x.total = None;
        x.send_started_ms = cx.now_ms();
        // Re-root defensively: a `send()` on an XHR whose root was released
        // (constructor → open → terminal → open → send) must be kept alive.
        x.wrapper = Some(this.wrapper.clone());
    }
    cx.state.pending_net.borrow_mut().insert(
        id,
        PendingNet::Xhr {
            xhr: Rc::clone(&this),
        },
    );
    arm_timeout(cx, &this)?;

    fire_progress(cx, &this, Target::Xhr, "loadstart", 0.0, None);
    // The upload sequence starts here. Its completion is reported when the
    // response head proves the body went out — see `upload_finished`.
    if !this.borrow().upload_complete {
        fire_progress(cx, &this, Target::Upload, "loadstart", 0.0, Some(body_len));
    }
    Ok(())
}

pub(crate) fn abort(cx: &BindCx<'_>, this: XhrRef) -> Result<(), JsThrow> {
    terminate(cx, &this);
    // "If state is opened with the send() flag set, headers received, or
    // loading, run the request error steps for abort." An XHR that was never
    // sent fires **nothing** — this used to fire a full sequence at a fresh
    // object.
    let fire = {
        let x = this.borrow();
        (x.ready_state == OPENED && x.send_flag)
            || matches!(x.ready_state, HEADERS_RECEIVED | LOADING)
    };
    if fire {
        request_error(cx, &this, "abort");
    }
    // "If state is done, then set state to unsent and set response to a network
    // error." An aborted request leaves no observable readyState or response
    // behind — `status` used to still read 200 with the partial body intact.
    let mut x = this.borrow_mut();
    if x.ready_state == DONE {
        x.ready_state = UNSENT;
        x.set_network_error();
    }
    Ok(())
}

pub(crate) fn override_mime_type(
    cx: &BindCx<'_>,
    this: XhrRef,
    mime: String,
) -> Result<(), JsThrow> {
    if matches!(this.borrow().ready_state, LOADING | DONE) {
        return Err(cx.dom_throw(
            DomExceptionKind::InvalidStateError,
            "XMLHttpRequest.overrideMimeType: the response is already being delivered",
        ));
    }
    this.borrow_mut().override_mime = Some(mime);
    Ok(())
}

// === Response headers ===

/// Response headers script may never see. `Set-Cookie` reaches the bindings on
/// a same-origin (`basic`) response because the net layer forwards the whole
/// header map — filtering it here is what stops a session cookie marked
/// `HttpOnly` at the *cookie jar* from being read straight off the response.
fn is_hidden_response_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("set-cookie") || name.eq_ignore_ascii_case("set-cookie2")
}

/// The script-visible response headers as a `HeadersData`, so that combining
/// and sorting reuse the `Headers` interface's implementation rather than a
/// second copy of the same rules.
fn visible_headers(x: &XhrData) -> crate::netdata::HeadersData {
    let visible: Vec<(String, String)> = x
        .response_headers
        .iter()
        .filter(|(n, _)| !is_hidden_response_header(n))
        .cloned()
        .collect();
    crate::netdata::HeadersData::from_pairs(&visible)
}

pub(crate) fn get_response_header(
    _cx: &BindCx<'_>,
    this: XhrRef,
    name: String,
) -> Result<Option<String>, JsThrow> {
    Ok(visible_headers(&this.borrow()).get(&name))
}

pub(crate) fn get_all_response_headers(_cx: &BindCx<'_>, this: XhrRef) -> Result<String, JsThrow> {
    let mut out = String::new();
    for (name, value) in visible_headers(&this.borrow()).sorted_combined() {
        out.push_str(&name);
        out.push_str(": ");
        out.push_str(&value);
        out.push_str("\r\n");
    }
    Ok(out)
}

// === responseType / response / responseText / responseXML ===

pub(crate) fn response_type(_cx: &BindCx<'_>, this: XhrRef) -> Result<String, JsThrow> {
    Ok(this.borrow().response_type.clone())
}

pub(crate) fn set_response_type(
    cx: &BindCx<'_>,
    this: XhrRef,
    value: String,
) -> Result<(), JsThrow> {
    if matches!(this.borrow().ready_state, LOADING | DONE) {
        return Err(cx.dom_throw(
            DomExceptionKind::InvalidStateError,
            "XMLHttpRequest.responseType: the response is already being delivered",
        ));
    }
    // An enumerated attribute ignores a value outside its set, leaving the
    // previous value in place rather than throwing.
    if RESPONSE_TYPES.contains(&value.as_str()) {
        this.borrow_mut().response_type = value;
    }
    Ok(())
}

pub(crate) fn response_text(cx: &BindCx<'_>, this: XhrRef) -> Result<String, JsThrow> {
    let x = this.borrow();
    if !matches!(x.response_type.as_str(), "" | "text") {
        return Err(cx.dom_throw(
            DomExceptionKind::InvalidStateError,
            "XMLHttpRequest.responseText: responseType is not `` or `text`",
        ));
    }
    if !matches!(x.ready_state, LOADING | DONE) {
        return Ok(String::new());
    }
    Ok(decode_text(&x))
}

pub(crate) fn response(cx: &BindCx<'_>, this: XhrRef) -> Result<JsValue, JsThrow> {
    let (kind, state) = {
        let x = this.borrow();
        (x.response_type.clone(), x.ready_state)
    };
    if matches!(kind.as_str(), "" | "text") {
        if !matches!(state, LOADING | DONE) {
            return Ok(JsValue::String(String::new()));
        }
        return Ok(JsValue::String(decode_text(&this.borrow())));
    }
    // Every object-valued responseType is only available once the body is
    // complete — a partially-arrived body cannot be parsed as anything.
    if state != DONE {
        return Ok(JsValue::Null);
    }
    if let Some(cached) = this.borrow().response_object.clone() {
        return Ok(cached.unwrap_or(JsValue::Null));
    }
    let computed = match kind.as_str() {
        "arraybuffer" => Some(array_buffer_response(cx, &this)?),
        "blob" => Some(blob_response(cx, &this)?),
        "json" => json_response(cx, &this)?,
        "document" => document_response(cx, &this)?,
        // Unreachable: `set_response_type` refuses anything else.
        _ => None,
    };
    this.borrow_mut().response_object = Some(computed.clone());
    Ok(computed.unwrap_or(JsValue::Null))
}

pub(crate) fn response_xml(cx: &BindCx<'_>, this: XhrRef) -> Result<Option<NodeId>, JsThrow> {
    {
        let x = this.borrow();
        if !matches!(x.response_type.as_str(), "" | "document") {
            return Err(cx.dom_throw(
                DomExceptionKind::InvalidStateError,
                "XMLHttpRequest.responseXML: responseType is not `` or `document`",
            ));
        }
        if x.ready_state != DONE {
            return Ok(None);
        }
    }
    // A cached document from a *previous* document — navigation replaces the
    // arena — is a snapshot that no longer names anything. Drop it and reparse
    // rather than hand back an id into the new tree.
    let stale = this
        .borrow()
        .response_document
        .is_some_and(|id| cx.state.dom.borrow().get(id).is_none());
    if stale {
        let mut x = this.borrow_mut();
        x.response_document = None;
        x.response_object = None;
    }
    if let Some(id) = this.borrow().response_document {
        return Ok(Some(id));
    }
    if this.borrow().response_object.is_some() {
        // Already computed and it was not a document (unsupported MIME type).
        return Ok(None);
    }
    let computed = document_response(cx, &this)?;
    this.borrow_mut().response_object = Some(computed);
    Ok(this.borrow().response_document)
}

/// `responseText`'s decoding: the response bytes read with the **final
/// charset** — the charset of an `overrideMimeType()` value if it named one,
/// else the response's own, else UTF-8. Previously this was an unconditional
/// lossy UTF-8 read, which mangled every non-UTF-8 response.
fn decode_text(x: &XhrData) -> String {
    let label = x
        .override_mime
        .as_deref()
        .and_then(charset_from_content_type)
        .or_else(|| {
            x.response_content_type()
                .and_then(charset_from_content_type)
        })
        .unwrap_or("utf-8");
    decode_with_charset(&x.response_body, label)
}

/// The MIME type's essence (type/subtype, no parameters), lowercased.
fn mime_essence(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// The spec's "final MIME type": the `overrideMimeType()` value if there is
/// one, else the response's `Content-Type`.
fn final_mime(x: &XhrData) -> String {
    match &x.override_mime {
        Some(mime) => mime_essence(mime),
        None => mime_essence(x.response_content_type().unwrap_or("")),
    }
}

fn array_buffer_response(cx: &BindCx<'_>, this: &XhrRef) -> Result<JsValue, JsThrow> {
    cx.bytes_to_array_buffer(&this.borrow().response_body)
}

/// The blob response: the body bytes with the **final** MIME type, so an
/// `overrideMimeType()` call is reflected in `xhr.response.type` exactly as it
/// is in `responseText`'s decoding.
fn blob_response(cx: &BindCx<'_>, this: &XhrRef) -> Result<JsValue, JsThrow> {
    let (mime, bytes) = {
        let x = this.borrow();
        (final_mime(&x), x.response_body.clone())
    };
    cx.new_blob(std::rc::Rc::new(BlobData::new(
        bytes,
        normalize_type(&mime),
    )))
}

/// The JSON response. Per the spec this is a **UTF-8** decode of the bytes, not
/// a charset-aware one, and a parse failure is `null` rather than a throw.
fn json_response(cx: &BindCx<'_>, this: &XhrRef) -> Result<Option<JsValue>, JsThrow> {
    let text = String::from_utf8_lossy(&this.borrow().response_body).into_owned();
    let global = cx.with_global()?;
    let json = cx.scope.get(&global, "JSON").map_err(JsThrow::from)?;
    let JsValue::Object(json) = &json else {
        return Ok(None);
    };
    let parse = cx.scope.get(json, "parse").map_err(JsThrow::from)?;
    Ok(cx
        .scope
        .call(&parse, &JsValue::Undefined, &[JsValue::String(text)])
        .ok())
}

/// The document response: `null` unless the final MIME type is an HTML or XML
/// one. Parsing reuses `DOMParser`, which is the only full-document parse entry
/// point there is — so an XHR document and a `DOMParser` document are the same
/// kind of object, with the same ADR-0017 caveat that XML is parsed by the HTML
/// parser.
fn document_response(cx: &BindCx<'_>, this: &XhrRef) -> Result<Option<JsValue>, JsThrow> {
    let (mime, text) = {
        let x = this.borrow();
        (final_mime(&x), decode_text(&x))
    };
    let ty = match mime.as_str() {
        "text/html" => "text/html",
        "text/xml" | "application/xml" | "application/xhtml+xml" | "image/svg+xml" => &mime,
        // Any other `+xml` type is an XML MIME type; the parser knows the four
        // names above, so it is parsed as generic XML.
        other if other.ends_with("+xml") => "application/xml",
        // "" is the spec's own case: a response with no `Content-Type` at all
        // is parsed as HTML.
        "" => "text/html",
        _ => return Ok(None),
    };
    let document = crate::imp::dom_parser::parse_from_string(cx, 0, text, ty.to_owned())?;
    let wrapper = cx.node_to_js(document)?;
    this.borrow_mut().response_document = Some(document);
    Ok(Some(wrapper))
}

// === Timeout ===

/// Arms (or re-arms) the `timeout` timer against the moment `send()` started.
///
/// The timeout is a page-side timer plus `NetService::abort`, not per-request
/// net plumbing: observably correct, at the cost of the socket possibly reading
/// on to completion before being discarded (ADR-0024).
fn arm_timeout(cx: &BindCx<'_>, this: &XhrRef) -> Result<(), JsThrow> {
    if let Some(id) = this.borrow_mut().timeout_timer.take() {
        cx.state.hooks.clear_timer(id);
    }
    let (timeout, started) = {
        let x = this.borrow();
        (x.timeout, x.send_started_ms)
    };
    if timeout == 0 {
        return Ok(());
    }
    let remaining = (f64::from(timeout) - (cx.now_ms() - started)).max(0.0);
    // The callback holds a `Weak`: a pending timer must not be what keeps a
    // collected XHR's state alive.
    let weak = Rc::downgrade(&this.data);
    let host: HostFn = Rc::new(move |scope, _call| {
        let cx = BindCx {
            scope,
            state: crate::cx::world_state(scope)?,
        };
        if let Some(data) = weak.upgrade() {
            timeout_fired(&cx, &data);
        }
        Ok(JsValue::Undefined)
    });
    let func = cx
        .scope
        .new_function("XMLHttpRequest timeout", 0, host)
        .map_err(JsThrow::from)?;
    let id = cx.state.hooks.schedule_timer(
        cx.state.id,
        JsValue::Object(func),
        Vec::new(),
        remaining,
        false,
    );
    this.borrow_mut().timeout_timer = Some(id);
    Ok(())
}

/// The timeout fired: terminate the fetch and run the request error steps for
/// `timeout`. A request that already finished (the timer outran its `clear`)
/// has no send() flag and does nothing.
fn timeout_fired(cx: &BindCx<'_>, data: &Rc<RefCell<XhrData>>) {
    let Some(this) = rehydrate(data) else {
        return;
    };
    if !this.borrow().send_flag {
        return;
    }
    terminate(cx, &this);
    request_error(cx, &this, "timeout");
}

/// Rebuilds an [`XhrRef`] from the state alone, for the paths that reach an XHR
/// without a receiver (the timer callback, net event delivery). The wrapper is
/// the live self-root, which is set for exactly as long as those paths can run.
pub(crate) fn rehydrate(data: &Rc<RefCell<XhrData>>) -> Option<XhrRef> {
    let wrapper = data.borrow().wrapper.clone()?;
    Some(XhrRef {
        data: Rc::clone(data),
        wrapper,
    })
}

// === Shared transition helpers, also driven by `deliver_net_event` ===

/// Cancels the in-flight request (net side and bookkeeping) and disarms the
/// timeout. Idempotent.
pub(crate) fn terminate(cx: &BindCx<'_>, this: &XhrRef) {
    let (id, timer) = {
        let mut x = this.borrow_mut();
        (x.request_id.take(), x.timeout_timer.take())
    };
    if let Some(id) = id {
        cx.state.hooks.abort(id);
        cx.state.pending_net.borrow_mut().remove(&id);
    }
    if let Some(timer) = timer {
        cx.state.hooks.clear_timer(timer);
    }
}

/// The spec's **request error steps**, shared by `abort`, `error` and
/// `timeout`: the response becomes a network error, the state becomes DONE, and
/// exactly one of the three terminal events fires — always followed by
/// `loadend`.
pub(crate) fn request_error(cx: &BindCx<'_>, this: &XhrRef, event_type: &str) {
    {
        let mut x = this.borrow_mut();
        x.ready_state = DONE;
        x.send_flag = false;
        x.set_network_error();
    }
    // The upload half, if the body never got a completion, reports the same
    // terminal event first.
    let upload_pending = !this.borrow().upload_complete;
    if upload_pending {
        this.borrow_mut().upload_complete = true;
        fire_progress(cx, this, Target::Upload, event_type, 0.0, None);
        fire_progress(cx, this, Target::Upload, "loadend", 0.0, None);
    }
    fire_plain(cx, this, "readystatechange");
    fire_progress(cx, this, Target::Xhr, event_type, 0.0, None);
    fire_progress(cx, this, Target::Xhr, "loadend", 0.0, None);
    release_root(this);
}

/// Marks the request body as fully transmitted and fires the upload object's
/// completion sequence. Called when the response head arrives — the earliest
/// point at which the body demonstrably went out.
pub(crate) fn upload_finished(cx: &BindCx<'_>, this: &XhrRef) {
    if this.borrow().upload_complete {
        return;
    }
    let total = {
        let mut x = this.borrow_mut();
        x.upload_complete = true;
        x.upload_total
    };
    // The body is handed to hyper whole, so the only honest report is 100%:
    // `loaded == total ==` the byte count `send()` recorded, and
    // `lengthComputable` true. Reporting `(0, None)` said the opposite.
    fire_progress(cx, this, Target::Upload, "progress", total, Some(total));
    fire_progress(cx, this, Target::Upload, "load", total, Some(total));
    fire_progress(cx, this, Target::Upload, "loadend", total, Some(total));
}

/// The successful terminal sequence: DONE, then `load`, then `loadend`.
pub(crate) fn request_done(cx: &BindCx<'_>, this: &XhrRef) {
    {
        let mut x = this.borrow_mut();
        x.ready_state = DONE;
        x.send_flag = false;
    }
    fire_plain(cx, this, "readystatechange");
    fire_transfer_progress(cx, this, "load");
    fire_transfer_progress(cx, this, "loadend");
    release_root(this);
}

/// One chunk of the body arrived: enter LOADING (nothing used to write state 3
/// at all) and report progress. Written against a chunk *stream* even though
/// the net layer currently emits exactly one `Chunk` per response — when it
/// learns to stream, this loop needs no change (ADR-0004, ADR-0024).
pub(crate) fn chunk_received(cx: &BindCx<'_>, this: &XhrRef, data: &[u8]) {
    {
        let mut x = this.borrow_mut();
        x.response_body.extend_from_slice(data);
        x.loaded += data.len() as f64;
        x.ready_state = LOADING;
        // A new body invalidates any response object computed from the old one.
        x.response_object = None;
        x.response_document = None;
    }
    fire_plain(cx, this, "readystatechange");
    fire_transfer_progress(cx, this, "progress");
}

/// Releases the self-root on a terminal transition, so a finished and
/// script-abandoned XHR can be collected. `open()` puts it back.
///
/// The send() flag check is load-bearing: a `load`/`error` listener may have
/// reopened and re-sent this very XHR from inside the sequence that is
/// terminating, and dropping the root then would silently un-root a request
/// that has only just started.
fn release_root(this: &XhrRef) {
    let mut x = this.borrow_mut();
    if !x.send_flag {
        x.wrapper = None;
    }
}

// === Event firing ===

/// Which of the two event targets an XHR event goes to.
#[derive(Clone, Copy, PartialEq)]
enum Target {
    Xhr,
    Upload,
}

/// This XHR's (or its upload object's) identity as an event target, and the
/// wrapper `event.target` must hand back. Absent when the object has no live
/// wrapper — a released root, or an upload object script never asked for.
fn target_of(this: &XhrRef, target: Target) -> Option<(EventTargetKey, JsValue)> {
    let x = this.borrow();
    match target {
        Target::Xhr => Some((EventTargetKey::Host(x.slab_key), x.wrapper.clone()?)),
        Target::Upload => Some((EventTargetKey::Host(x.upload_key), x.upload.clone()?)),
    }
}

/// Fires one XHR event through the real dispatch machinery.
///
/// The event is a genuine `Event` object, not the `{type, target}` stand-in
/// this used to build: `preventDefault`, `stopPropagation`, `currentTarget`,
/// `isTrusted` and `instanceof Event` all work, and every listener option
/// (`capture`, `once`, `passive`) is honoured because the shared registry is
/// doing the work.
fn fire_at(
    cx: &BindCx<'_>,
    this: &XhrRef,
    target: Target,
    interface: &'static str,
    mut data: EventData,
) {
    let Some((key, _wrapper)) = target_of(this, target) else {
        return;
    };
    let event_type = data.event_type.clone();
    data.is_trusted = true;
    data.time_stamp = cx.now_ms();
    let data = cx.new_event_data(interface, data);
    if let Err(e) = crate::events::dispatch_event(cx, key, &data) {
        cx.warn(&format!(
            "XMLHttpRequest `{event_type}` dispatch failed: {e:?}"
        ));
    }
}

/// `readystatechange` — the one XHR event that is a plain `Event`.
pub(crate) fn fire_plain(cx: &BindCx<'_>, this: &XhrRef, event_type: &str) {
    fire_at(
        cx,
        this,
        Target::Xhr,
        "Event",
        EventData::new(
            event_type.to_owned(),
            /* bubbles */ false,
            /* cancelable */ false,
            /* composed */ false,
        ),
    );
}

/// Fires a `ProgressEvent`. `total` is `Some` only when the transfer length is
/// actually known — `lengthComputable` follows from that, and a `total` is
/// never fabricated from the bytes seen so far.
fn fire_progress(
    cx: &BindCx<'_>,
    this: &XhrRef,
    target: Target,
    event_type: &str,
    loaded: f64,
    total: Option<f64>,
) {
    let data = crate::imp::progress_event::event_data(
        event_type,
        total.is_some(),
        loaded,
        total.unwrap_or(0.0),
    );
    fire_at(cx, this, target, "ProgressEvent", data);
}

/// A download-side progress event, reading the counters off the XHR.
fn fire_transfer_progress(cx: &BindCx<'_>, this: &XhrRef, event_type: &str) {
    let (loaded, total) = {
        let x = this.borrow();
        (x.loaded, x.total)
    };
    fire_progress(cx, this, Target::Xhr, event_type, loaded, total);
}

/// Method normalization (Fetch): the four common methods are uppercased, any
/// other token is sent verbatim — `PATCH` is a real method and `patch` is not.
fn normalize_method(method: &str) -> String {
    if matches!(
        method.to_ascii_uppercase().as_str(),
        "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT" | "CONNECT" | "TRACE" | "TRACK"
    ) {
        method.to_ascii_uppercase()
    } else {
        method.to_owned()
    }
}

/// The `onreadystatechange` handler property. The six shared ones live on
/// `XMLHttpRequestEventTarget`; this one is `XMLHttpRequest`'s alone.
pub(crate) fn onreadystatechange(cx: &BindCx<'_>, this: XhrRef) -> Result<JsValue, JsThrow> {
    Ok(crate::imp::xhr_event_target::get(
        cx,
        EventTargetKey::Host(this.borrow().slab_key),
        "readystatechange",
    ))
}

pub(crate) fn set_onreadystatechange(
    cx: &BindCx<'_>,
    this: XhrRef,
    value: JsValue,
) -> Result<(), JsThrow> {
    crate::imp::xhr_event_target::set(
        cx,
        EventTargetKey::Host(this.borrow().slab_key),
        "readystatechange",
        value,
    );
    Ok(())
}
