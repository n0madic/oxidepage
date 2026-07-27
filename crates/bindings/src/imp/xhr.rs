//! `XMLHttpRequest` implementation.
//!
//! `XMLHttpRequest` is a real `EventTarget`: its listeners and `onX` handlers
//! live in the shared registries keyed by
//! [`crate::events::EventTargetKey::Host`], and its events go through
//! [`crate::events::dispatch_event`] like any other. The slab key doubles as
//! the event-target identity, the same scheme `new EventTarget()` uses.

use std::cell::RefCell;
use std::rc::Rc;

use oxidepage_base::DomExceptionKind;
use oxidepage_js::{HostCall, JsThrow, JsValue};
use oxidepage_net::{Credentials, NetRequest, RequestMode};

use crate::cx::BindCx;
use crate::events::EventTargetKey;
use crate::netdata::{PendingNet, XhrData, is_valid_header_name, is_valid_header_value};
use crate::state::HostData;

type Xhr = Rc<RefCell<XhrData>>;

const OPENED: u16 = 1;
const DONE: u16 = 4;

pub(crate) fn constructor(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    let xhr = Rc::new(RefCell::new(XhrData::default()));
    let wrapper = cx.new_net_object("XMLHttpRequest", HostData::Xhr(Rc::clone(&xhr)))?;
    {
        let mut x = xhr.borrow_mut();
        // The slab key is this object's event-target identity for its whole
        // life; every listener and handler is filed under it.
        x.slab_key = cx.slab_key(&wrapper).unwrap_or_default();
        x.wrapper = Some(wrapper.clone());
    }
    Ok(wrapper)
}

pub(crate) fn ready_state(_cx: &BindCx<'_>, this: Xhr) -> Result<f64, JsThrow> {
    Ok(f64::from(this.borrow().ready_state))
}

pub(crate) fn status(_cx: &BindCx<'_>, this: Xhr) -> Result<f64, JsThrow> {
    Ok(f64::from(this.borrow().status))
}

pub(crate) fn status_text(_cx: &BindCx<'_>, this: Xhr) -> Result<String, JsThrow> {
    Ok(this.borrow().status_text.clone())
}

pub(crate) fn response_type(_cx: &BindCx<'_>, this: Xhr) -> Result<String, JsThrow> {
    Ok(this.borrow().response_type.clone())
}

pub(crate) fn with_credentials(_cx: &BindCx<'_>, this: Xhr) -> Result<bool, JsThrow> {
    Ok(this.borrow().with_credentials)
}

pub(crate) fn set_with_credentials(
    _cx: &BindCx<'_>,
    this: Xhr,
    value: bool,
) -> Result<(), JsThrow> {
    this.borrow_mut().with_credentials = value;
    Ok(())
}

pub(crate) fn set_response_type(_cx: &BindCx<'_>, this: Xhr, value: String) -> Result<(), JsThrow> {
    this.borrow_mut().response_type = value;
    Ok(())
}

pub(crate) fn response_text(_cx: &BindCx<'_>, this: Xhr) -> Result<String, JsThrow> {
    Ok(String::from_utf8_lossy(&this.borrow().response_body).into_owned())
}

pub(crate) fn response(cx: &BindCx<'_>, this: Xhr) -> Result<JsValue, JsThrow> {
    let (kind, text) = {
        let x = this.borrow();
        (
            x.response_type.clone(),
            String::from_utf8_lossy(&x.response_body).into_owned(),
        )
    };
    match kind.as_str() {
        "json" => {
            let global = cx.with_global()?;
            let json = cx.scope.get(&global, "JSON").map_err(JsThrow::from)?;
            if let JsValue::Object(json) = &json {
                let parse = cx.scope.get(json, "parse").map_err(JsThrow::from)?;
                return cx
                    .scope
                    .call(&parse, &JsValue::Undefined, &[JsValue::String(text)])
                    .or(Ok(JsValue::Null));
            }
            Ok(JsValue::Null)
        }
        _ => Ok(JsValue::String(text)),
    }
}

pub(crate) fn open(
    _cx: &BindCx<'_>,
    this: Xhr,
    method: String,
    url: String,
) -> Result<(), JsThrow> {
    let mut x = this.borrow_mut();
    x.method = method.to_ascii_uppercase();
    x.url = url;
    x.request_headers.clear();
    x.response_headers.clear();
    x.response_body.clear();
    x.status = 0;
    x.status_text.clear();
    x.ready_state = OPENED;
    drop(x);
    fire_event(_cx, &this, "readystatechange");
    Ok(())
}

pub(crate) fn set_request_header(
    cx: &BindCx<'_>,
    this: Xhr,
    name: String,
    value: String,
) -> Result<(), JsThrow> {
    // Normalize the value (strip surrounding HTTP whitespace) then reject an
    // invalid name/value — a CR/LF/NUL here would inject request headers.
    let value = value.trim();
    if !is_valid_header_name(&name) || !is_valid_header_value(value) {
        return Err(cx.dom_throw(
            DomExceptionKind::SyntaxError,
            "XMLHttpRequest.setRequestHeader: invalid header name or value",
        ));
    }
    this.borrow_mut()
        .request_headers
        .push((name, value.to_owned()));
    Ok(())
}

pub(crate) fn send(cx: &BindCx<'_>, this: Xhr, body: JsValue) -> Result<(), JsThrow> {
    let (method, mut headers, url, with_credentials) = {
        let x = this.borrow();
        (
            x.method.clone(),
            x.request_headers.clone(),
            x.url.clone(),
            x.with_credentials,
        )
    };
    if url.is_empty() {
        return Err(JsThrow::Type(
            "XMLHttpRequest.send: open() has not been called".into(),
        ));
    }
    let body_bytes = match crate::imp::body::extract(cx, &body)? {
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

    let doc_url = cx.state.dom.borrow().document_url().to_owned();
    let absolute = match url::Url::parse(&url) {
        Ok(u) => u.to_string(),
        Err(_) => url::Url::parse(&doc_url)
            .and_then(|base| base.join(&url))
            .map(|u| u.to_string())
            .map_err(|_| JsThrow::Type(format!("XMLHttpRequest: invalid URL `{url}`")))?,
    };
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
        method,
        url: absolute,
        headers,
        body: body_bytes,
        credentials,
        mode: RequestMode::Cors,
        referrer: Some(doc_url),
        initiator_origin,
        bypass_cache: false,
    };
    let id = cx.state.hooks.start_fetch(request);
    this.borrow_mut().request_id = Some(id);
    cx.state.pending_net.borrow_mut().insert(
        id,
        PendingNet::Xhr {
            xhr: Rc::clone(&this),
        },
    );
    Ok(())
}

pub(crate) fn abort(cx: &BindCx<'_>, this: Xhr) -> Result<(), JsThrow> {
    let id = this.borrow_mut().request_id.take();
    if let Some(id) = id {
        cx.state.hooks.abort(id);
        cx.state.pending_net.borrow_mut().remove(&id);
    }
    this.borrow_mut().ready_state = DONE;
    fire_event(cx, &this, "readystatechange");
    fire_event(cx, &this, "abort");
    fire_event(cx, &this, "loadend");
    let mut x = this.borrow_mut();
    x.ready_state = 0; // UNSENT
    // Terminal: release the self-referential wrapper root so an aborted,
    // script-abandoned request can be collected.
    x.wrapper = None;
    Ok(())
}

pub(crate) fn get_response_header(
    _cx: &BindCx<'_>,
    this: Xhr,
    name: String,
) -> Result<Option<String>, JsThrow> {
    let name = name.to_ascii_lowercase();
    let x = this.borrow();
    let values: Vec<&str> = x
        .response_headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(&name))
        .map(|(_, v)| v.as_str())
        .collect();
    Ok((!values.is_empty()).then(|| values.join(", ")))
}

pub(crate) fn get_all_response_headers(_cx: &BindCx<'_>, this: Xhr) -> Result<String, JsThrow> {
    let x = this.borrow();
    let mut out = String::new();
    for (name, value) in &x.response_headers {
        out.push_str(&format!("{}: {value}\r\n", name.to_ascii_lowercase()));
    }
    Ok(out)
}

/// The `onX` handler properties.
///
/// They are stored in the shared `event_handlers` registry keyed by this
/// object's `EventTargetKey::Host`, not on the `XhrData` — which is what puts
/// them on the same footing as `addEventListener` registrations, so
/// `invoke_listeners` runs them and `preventDefault` works from either.
macro_rules! handler {
    ($get:ident, $set:ident, $event_type:literal) => {
        pub(crate) fn $get(cx: &BindCx<'_>, this: Xhr) -> Result<JsValue, JsThrow> {
            Ok(cx
                .state
                .event_handlers
                .borrow()
                .get(&(target_key(&this), $event_type.to_owned()))
                .cloned()
                .unwrap_or(JsValue::Null))
        }
        pub(crate) fn $set(cx: &BindCx<'_>, this: Xhr, value: JsValue) -> Result<(), JsThrow> {
            let slot = (target_key(&this), $event_type.to_owned());
            let mut handlers = cx.state.event_handlers.borrow_mut();
            if value.is_nullish() {
                handlers.remove(&slot);
            } else {
                handlers.insert(slot, value);
            }
            Ok(())
        }
    };
}

/// This XHR's identity as an event target.
fn target_key(xhr: &Xhr) -> EventTargetKey {
    EventTargetKey::Host(xhr.borrow().slab_key)
}

handler!(
    onreadystatechange,
    set_onreadystatechange,
    "readystatechange"
);
handler!(onload, set_onload, "load");
handler!(onerror, set_onerror, "error");
handler!(onloadend, set_onloadend, "loadend");
handler!(onabort, set_onabort, "abort");

/// Fires one XHR event through the real dispatch machinery.
///
/// The event is a genuine `Event` object, not the `{type, target}` stand-in
/// this used to build: `preventDefault`, `stopPropagation`, `currentTarget`,
/// `isTrusted` and `instanceof Event` all work, and every listener option
/// (`capture`, `once`, `passive`) is honoured because the shared registry is
/// doing the work.
pub(crate) fn fire_event(cx: &BindCx<'_>, xhr: &Xhr, event_type: &str) {
    let key = target_key(xhr);
    // A wrapper is required for `event.target`; it is released on a terminal
    // readyState, so a reused XHR can legitimately have none.
    if xhr.borrow().wrapper.is_none() {
        return;
    }
    let mut data = crate::events::EventData::new(
        event_type.to_owned(),
        /* bubbles */ false,
        /* cancelable */ false,
        /* composed */ false,
    );
    data.is_trusted = true;
    data.time_stamp = cx.now_ms();
    let Ok((value, data)) = cx.new_event_object("Event", data) else {
        return;
    };
    if let Err(e) = crate::events::dispatch_event(cx, key, &value, &data) {
        cx.warn(&format!(
            "XMLHttpRequest `{event_type}` dispatch failed: {e:?}"
        ));
    }
}
