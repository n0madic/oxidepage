//! `WindowProxy`: the handle `window.open` returns (ADR-0027 D12).
//!
//! Everything here is either an atomic read or a fire-and-forget message. The
//! sibling lives on another thread with its own realm, and a getter that
//! blocked on a round trip would be a deadlock the first time two pages opened
//! each other — so nothing here waits for an answer.

use std::rc::Rc;
use std::sync::atomic::Ordering;

use oxidepage_base::DomExceptionKind;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::window_open::{WindowOp, WindowProxyData};

pub(crate) fn closed(_cx: &BindCx<'_>, this: Rc<WindowProxyData>) -> Result<bool, JsThrow> {
    Ok(this.window.closed.load(Ordering::Acquire))
}

pub(crate) fn close(_cx: &BindCx<'_>, this: Rc<WindowProxyData>) -> Result<(), JsThrow> {
    // Set the flag here as well as asking the sibling to go: a browser reports
    // `w.closed === true` on the very next line, and waiting for the other
    // thread to acknowledge would make that a race.
    this.window.closed.store(true, Ordering::Release);
    (this.window.ops)(WindowOp::Close);
    Ok(())
}

/// There is no window manager here, so focusing a browsing context has no
/// intrinsic effect. The embedder is *told* rather than obeyed — which is what
/// keeps this from being the silent no-op P6 forbids.
pub(crate) fn focus(_cx: &BindCx<'_>, this: Rc<WindowProxyData>) -> Result<(), JsThrow> {
    (this.window.ops)(WindowOp::Focus);
    Ok(())
}

/// Reading a sibling's `location` throws, exactly as it does for a cross-origin
/// `WindowProxy` in a browser — which is what this *is*: a separate browsing
/// context this realm cannot synchronously introspect.
pub(crate) fn location(cx: &BindCx<'_>, _this: Rc<WindowProxyData>) -> Result<JsValue, JsThrow> {
    Err(cx.dom_throw(
        DomExceptionKind::SecurityError,
        "Failed to read the 'location' property from 'WindowProxy': \
         cannot read the location of another browsing context",
    ))
}

/// `w.location = url` navigates the sibling. Resolved against the *opener's*
/// document, which is what HTML says and what needs no round trip.
pub(crate) fn set_location(
    cx: &BindCx<'_>,
    this: Rc<WindowProxyData>,
    value: JsValue,
) -> Result<(), JsThrow> {
    if this.window.closed.load(Ordering::Acquire) {
        return Ok(());
    }
    let url = cx.scope.coerce_string(&value)?;
    // Resolved against the opener's **current** document, read now — not
    // against a snapshot taken when `window.open` returned. The realm outlives
    // a navigation, so a proxy captured before one would otherwise keep
    // resolving against a URL this page has left, and send its sibling to a
    // different origin than the script asked for.
    let base = cx.state.dom.borrow().document_url().to_owned();
    let resolved = crate::window_open::resolve_against(&base, &url);
    (this.window.ops)(WindowOp::Navigate(resolved));
    Ok(())
}
