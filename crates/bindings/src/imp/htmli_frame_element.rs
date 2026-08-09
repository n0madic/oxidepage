//! `HTMLIFrameElement`: attribute reflection plus the one member that reaches
//! into the nested browsing context, `contentDocument` (ADR-0035 D4).
//!
//! `src` and `srcdoc` are **not installed yet**: they navigate the frame, and
//! that goes through the ordinary attribute path — the DOM queues the change
//! and the page's event loop performs the load — never synchronously from a
//! setter, which is called with `dom` borrowed. Until that path exists they
//! would reflect and load nothing, which is the silent no-op P6 forbids.

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::imp::reflect::string_reflector;

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
fn same_origin(a: &str, b: &str) -> bool {
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
