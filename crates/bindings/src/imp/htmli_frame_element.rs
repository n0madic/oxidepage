//! `HTMLIFrameElement`: attribute reflection plus the one member that reaches
//! into the nested browsing context, `contentDocument` (ADR-0035 D4).
//!
//! Writing `src` or `srcdoc` navigates the frame, and does so through the
//! ordinary attribute path: the DOM queues the change and the page's event
//! loop performs the load. Never synchronously from the setter — a load
//! re-enters the event loop, and the setter is called with `dom` borrowed
//! (ADR-0035 D5).

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::imp::reflect::{string_reflector, url_reflector};

url_reflector!(src, set_src, "src");
string_reflector!(srcdoc, set_srcdoc, "srcdoc");
string_reflector!(name, set_name, "name");
string_reflector!(width, set_width, "width");
string_reflector!(height, set_height, "height");
string_reflector!(referrer_policy, set_referrer_policy, "referrerpolicy");

/// The frame's `Document`, or `null` when there is no browsing context or the
/// frame is cross-origin.
///
/// The arena is shared, so this is a *real* Document the caller can walk and
/// mutate — the wrapper it gets is minted in the accessing realm, which is what
/// makes it work without any value crossing a runtime boundary (ADR-0035 D4).
pub(crate) fn content_document(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    let dom = cx.state.dom.borrow();
    let Some(document) = dom.content_document(this) else {
        return Ok(None);
    };
    // HTML returns null rather than throwing for a cross-origin frame.
    let here = dom.document_url_of(cx.state.frame.document());
    let there = dom.document_url_of(document);
    Ok(same_origin(here, there).then_some(document))
}

/// The frame's `WindowProxy`, or `null` when there is no browsing context.
///
/// Available cross-origin, exactly as in a browser: a `WindowProxy` is the one
/// object HTML lets you hold across an origin boundary — its *members* are what
/// the origin check gates, not the handle itself.
pub(crate) fn content_window(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    let document = cx.state.dom.borrow().content_document(this);
    let Some(document) = document else {
        return Ok(JsValue::Null);
    };
    match cx.state.frame.global.frame_of_document(document) {
        Some(state) => cx.new_frame_proxy(state.frame()),
        None => Ok(JsValue::Null),
    }
}

/// Whether two document URLs share an origin, compared as
/// `(scheme, host, port)`.
///
/// Deliberately not `Url::origin()`: that yields an *opaque* origin for
/// `file:`, so two files of the same directory would read as cross-origin and
/// a `file://` page could not reach its own frames — the same reason
/// `pushState` compares by parts (ADR-0022 §4).
///
/// `about:blank` and `srcdoc` frames inherit the embedder's origin at load, so
/// by the time this runs their URL is already the right one to compare.
pub(crate) fn same_origin(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (Ok(a), Ok(b)) = (url::Url::parse(a), url::Url::parse(b)) else {
        return false;
    };
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// A document URL reduced to `scheme://host[:port]`, which is what
/// `MessageEvent.origin` reports. `"null"` for anything without a host —
/// `about:blank` inherits its embedder's URL at load, so it never lands here.
pub(crate) fn origin_of(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return "null".to_owned();
    };
    match parsed.host_str() {
        Some(host) => match parsed.port() {
            Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
            None => format!("{}://{host}", parsed.scheme()),
        },
        None => "null".to_owned(),
    }
}
