//! `Window` methods backed by the realm global object.

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::dialog::{DialogKind, DialogRequest, DialogResponse};
use crate::events::EventTargetKey;
use crate::state::PendingNavigation;
use crate::window_open::OpenWindowRequest;

/// `window.name`: this browsing context's name (ADR-0035 D10).
pub(crate) fn name(cx: &BindCx<'_>, _this: EventTargetKey) -> Result<String, JsThrow> {
    Ok(cx.state.frame.name())
}

pub(crate) fn set_name(
    cx: &BindCx<'_>,
    _this: EventTargetKey,
    value: String,
) -> Result<(), JsThrow> {
    cx.state.frame.set_name(&value);
    Ok(())
}

/// A window that is running its own script is, definitionally, open.
pub(crate) fn closed(_cx: &BindCx<'_>, _this: EventTargetKey) -> Result<bool, JsThrow> {
    Ok(false)
}

/// HTML's "close steps", which begin by checking that the browsing context was
/// opened by script and return if it was not.
///
/// This engine tracks no opener, so that check can only fail: the call is
/// ignored, and *reported*, which is what keeps it out of the silent-no-op
/// category P6 rules out. A sibling opened by `window.open` is closed through
/// its `WindowProxy`, which does have the handle to do it.
pub(crate) fn close(cx: &BindCx<'_>, _this: EventTargetKey) -> Result<(), JsThrow> {
    cx.warn(
        "window.close(): ignored — this engine tracks no opener, so a window \
         cannot tell whether script opened it (HTML ignores the call unless it did)",
    );
    Ok(())
}

pub(crate) fn match_media(
    cx: &BindCx<'_>,
    _this: EventTargetKey,
    query: String,
) -> Result<JsValue, JsThrow> {
    let matches = cx.state.style.borrow().media_query_matches(&query);
    cx.new_media_query_list(query, matches)
}

/// HTML §7.2.2 "The `window.open()` method", as far as this engine goes
/// (ADR-0027 D12).
///
/// Returns `null` when the embedder cannot open a browsing context — which is
/// a bare `Page`, and which is also what a browser returns for a blocked
/// popup. That is why the hook is `Option`-returning rather than absent: the
/// method exists and answers honestly instead of pretending.
pub(crate) fn open(
    cx: &BindCx<'_>,
    _this: EventTargetKey,
    url: String,
    target: String,
    features: String,
) -> Result<JsValue, JsThrow> {
    // Resolve, then **drop the borrow** before the hook: it is embedder code
    // called with JavaScript on the stack, exactly like the dialog handler.
    let (opener_url, resolved) = {
        let dom = cx.state.dom.borrow();
        let opener_url = dom.document_url().to_owned();
        let resolved =
            (!url.is_empty()).then(|| crate::window_open::resolve_against(&opener_url, &url));
        (opener_url, resolved)
    };

    // HTML: "If target is the empty string, then set target to `_blank`." An
    // explicit `window.open(url, "")` therefore opens a page — unlike an `<a>`
    // with an empty `target`, which navigates in place.
    let target = if target.is_empty() {
        "_blank".to_owned()
    } else {
        target
    };
    // `_self`/`_parent`/`_top` and a *named* context that exists all name a
    // browsing context of this page: HTML says navigate it and return it, not
    // open a sibling. `window.open(url, '_self')` is a common "navigate me"
    // idiom, and opening a page for it would leave the caller sitting where it
    // was (ADR-0035 D10).
    if let Some(context) = crate::window_open::resolve_target(&cx.state.frame, &target) {
        // HTML §7.2.2: "If url is not the empty string, then ... navigate".
        // An empty URL leaves the existing document alone — which matters,
        // because `window.open('', '_self'); window.close();` is a widespread
        // self-close shim, and blanking the document there would destroy a live
        // page's DOM, state and listeners. That an omitted URL means
        // `about:blank` applies to a browsing context being *created*, not to
        // one that already has a document.
        if let Some(url) = resolved {
            context.request_navigation(PendingNavigation::Load {
                url,
                replace: false,
                body: None,
                reload: false,
                download: None,
            });
        }
        // The calling window *is* its own `WindowProxy`, which is what a
        // browser returns for `_self`; for another context of this page the
        // proxy of that context is the honest answer.
        if context.frame() == cx.state.frame.frame() {
            return Ok(JsValue::Object(cx.with_js(|js| js.global.clone())?));
        }
        return cx.new_frame_proxy(context.frame());
    }

    let request = OpenWindowRequest {
        url: resolved,
        target,
        features,
        opener_url: opener_url.clone(),
    };
    match cx.state.hooks.open_window(request) {
        Some(window) => cx.new_window_proxy(window),
        None => Ok(JsValue::Null),
    }
}

/// HTML §8.9 "User prompts", the shared half: build the request and let the
/// embedder answer it.
///
/// The `dom` borrow is taken for the URL and **dropped before the hook runs**:
/// the handler is embedder code called with JavaScript on the stack, and one
/// that touched the page under a live borrow would panic. Nothing here
/// flushes layout, for the same reason.
fn open_dialog(
    cx: &BindCx<'_>,
    kind: DialogKind,
    message: String,
    default_value: String,
) -> DialogResponse {
    let url = cx.state.dom.borrow().document_url().to_owned();
    cx.state.hooks.run_dialog(DialogRequest {
        kind,
        message,
        default_value,
        url,
    })
}

pub(crate) fn alert(
    cx: &BindCx<'_>,
    _this: EventTargetKey,
    message: String,
) -> Result<(), JsThrow> {
    // `alert` has no answer to report: whichever way it was closed, it returns.
    open_dialog(cx, DialogKind::Alert, message, String::new());
    Ok(())
}

pub(crate) fn confirm(
    cx: &BindCx<'_>,
    _this: EventTargetKey,
    message: String,
) -> Result<bool, JsThrow> {
    Ok(open_dialog(cx, DialogKind::Confirm, message, String::new()).accepted())
}

pub(crate) fn prompt(
    cx: &BindCx<'_>,
    _this: EventTargetKey,
    message: String,
    default_value: String,
) -> Result<Option<String>, JsThrow> {
    match open_dialog(cx, DialogKind::Prompt, message, default_value.clone()) {
        DialogResponse::Dismiss => Ok(None),
        // Accepting without typing keeps the page's own default text, exactly
        // as pressing OK on a pre-filled prompt does.
        DialogResponse::Accept => Ok(Some(default_value)),
        DialogResponse::AcceptWith(text) => Ok(Some(text)),
    }
}
