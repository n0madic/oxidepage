//! `Window` methods backed by the realm global object.

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::dialog::{DialogKind, DialogRequest, DialogResponse};
use crate::events::EventTargetKey;

pub(crate) fn match_media(
    cx: &BindCx<'_>,
    _this: EventTargetKey,
    query: String,
) -> Result<JsValue, JsThrow> {
    let matches = cx.state.style.borrow().media_query_matches(&query);
    cx.new_media_query_list(query, matches)
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
