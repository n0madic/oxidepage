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
// Reflected as a string rather than a `DOMTokenList`: only `allow-scripts` and
// `allow-same-origin` are enforced (ADR-0035 D11), and a `sandbox.add(...)`
// that appeared to grant `allow-forms` would be the fake P6 forbids. The
// attribute round-trips exactly, so a page can read what it wrote.
string_reflector!(sandbox, set_sandbox, "sandbox");
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
    let document = cx.state.dom.borrow().content_document(this);
    let Some(document) = document else {
        return Ok(None);
    };
    // HTML returns null rather than throwing for a frame this realm may not
    // reach. `same_origin_frame` is the one place that decides, so a sandbox
    // without `allow-same-origin` — an opaque origin — is refused here too
    // (ADR-0035 D11) rather than only by the members that ask it separately.
    let Some(state) = cx.state.frame.global.frame_of_document(document) else {
        return Ok(None);
    };
    Ok(cx.same_origin_frame(&state).then_some(document))
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
    let (Some(a), Some(b)) = (parse_for_origin(a), parse_for_origin(b)) else {
        return false;
    };
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Parses a document URL for an origin comparison, looking **through** a
/// `blob:` URL to the URL it was minted under (ADR-0042).
///
/// URL's origin of `blob:https://a.example/<uuid>` is `https://a.example`, so
/// a document loaded from one — `<iframe src=blob:…>`, a navigation to
/// `URL.createObjectURL(html)` — is same-origin with the page that made it.
/// Compared on its own scheme and host, it would be same-origin with nothing.
fn parse_for_origin(url: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.scheme() != "blob" {
        return Some(parsed);
    }
    let inner = url::Url::parse(parsed.path()).ok()?;
    matches!(inner.scheme(), "http" | "https" | "file").then_some(inner)
}

/// A document URL reduced to `scheme://host[:port]`, which is what
/// `MessageEvent.origin` reports. `"null"` for anything without a host —
/// `about:blank` inherits its embedder's URL at load, so it never lands here.
pub(crate) fn origin_of(url: &str) -> String {
    let Some(parsed) = parse_for_origin(url) else {
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
