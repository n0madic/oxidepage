//! `History`: the session history of the page's one browsing context.
//!
//! The entry list lives in [`crate::state::SessionHistory`]; `this` here is
//! only a brand token. Two halves, with different rules:
//!
//! * `pushState`/`replaceState` change the document URL **without** loading, so
//!   they are same-origin-restricted and happen inline.
//! * `go`/`back`/`forward` may have to *leave* the current document, which is a
//!   navigation and therefore a task — they queue a
//!   [`PendingNavigation::Traverse`] the page's event loop performs (ADR-0022).
//!
//! There is no bfcache, so traversing out of the current document reloads it.
//! `HistoryEntry::document_seq` is what tells the two cases apart: an entry
//! stamped with the current document's sequence is reachable without a load.

use oxidepage_base::DomExceptionKind;
use oxidepage_js::{JsThrow, JsValue};
use url::Url;

use crate::cx::BindCx;
use crate::state::PendingNavigation;

pub(crate) fn length(cx: &BindCx<'_>, _this: u64) -> Result<f64, JsThrow> {
    Ok(cx.state.history.borrow().len() as f64)
}

pub(crate) fn scroll_restoration(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    Ok(cx.state.history.borrow().scroll_restoration().to_owned())
}

/// Stored and reflected, nothing more: there is no bfcache whose scroll
/// position could be restored, and the value is observable, so recording it is
/// honest where silently discarding it would not be.
pub(crate) fn set_scroll_restoration(
    cx: &BindCx<'_>,
    _this: u64,
    value: String,
) -> Result<(), JsThrow> {
    cx.state.history.borrow_mut().set_scroll_restoration(&value);
    Ok(())
}

pub(crate) fn state(cx: &BindCx<'_>, _this: u64) -> Result<JsValue, JsThrow> {
    Ok(cx
        .state
        .history
        .borrow()
        .current()
        .map_or(JsValue::Null, |entry| entry.state.clone()))
}

/// `history.go(delta)`. `0` means "reload the current entry" — the one case
/// that is a real navigation even though the index does not move.
pub(crate) fn go(cx: &BindCx<'_>, _this: u64, delta: i32) -> Result<(), JsThrow> {
    if delta == 0 {
        return crate::imp::location::reload(cx, 0);
    }
    // Out of range is a silent no-op, per HTML's "traverse the history by a
    // delta": if there is no such entry, return.
    if cx.state.history.borrow().target_of(delta).is_none() {
        return Ok(());
    }
    cx.state
        .request_navigation(PendingNavigation::Traverse { delta });
    Ok(())
}

pub(crate) fn back(cx: &BindCx<'_>, this: u64) -> Result<(), JsThrow> {
    go(cx, this, -1)
}

pub(crate) fn forward(cx: &BindCx<'_>, this: u64) -> Result<(), JsThrow> {
    go(cx, this, 1)
}

pub(crate) fn push_state(
    cx: &BindCx<'_>,
    _this: u64,
    data: JsValue,
    _unused: String,
    url: Option<String>,
) -> Result<(), JsThrow> {
    shared_history_push(cx, data, url, /* replace */ false)
}

pub(crate) fn replace_state(
    cx: &BindCx<'_>,
    _this: u64,
    data: JsValue,
    _unused: String,
    url: Option<String>,
) -> Result<(), JsThrow> {
    shared_history_push(cx, data, url, /* replace */ true)
}

/// HTML's "shared history push/replace state steps": clone the state, check
/// the URL is same-origin, move the document URL, and push or overwrite.
fn shared_history_push(
    cx: &BindCx<'_>,
    data: JsValue,
    url: Option<String>,
    replace: bool,
) -> Result<(), JsThrow> {
    let method = if replace { "replaceState" } else { "pushState" };
    let current = cx.state.dom.borrow().document_url().to_owned();
    let target = match url {
        None => current.clone(),
        Some(url) => {
            let resolved = Url::parse(&current)
                .and_then(|base| base.join(&url))
                .map_err(|_| {
                    cx.dom_throw(
                        DomExceptionKind::SecurityError,
                        &format!("Failed to execute '{method}' on 'History': invalid URL `{url}`"),
                    )
                })?
                .to_string();
            if !same_origin(&resolved, &current) {
                return Err(cx.dom_throw(
                    DomExceptionKind::SecurityError,
                    &format!("Failed to execute '{method}' on 'History': cross-origin URL"),
                ));
            }
            resolved
        }
    };
    // Cloning may throw (DataCloneError on a DOM node), and must do so before
    // anything is mutated.
    let cloned = structured_clone(cx, data)?;

    cx.state.dom.borrow_mut().set_document_url(target.clone());
    let mut history = cx.state.history.borrow_mut();
    let seq = history.document_seq();
    if replace {
        history.replace(target, cloned, seq);
    } else {
        history.push(target, cloned, seq);
    }
    Ok(())
}

/// The (scheme, host, port) tuple rather than `Url::origin()`: `file:` and
/// other non-special schemes get a fresh *opaque* origin per parse, which would
/// make every local test document fail its own same-origin check.
pub(crate) fn same_origin(a: &str, b: &str) -> bool {
    match (Url::parse(a), Url::parse(b)) {
        (Ok(a), Ok(b)) => {
            a.scheme() == b.scheme()
                && a.host_str() == b.host_str()
                && a.port_or_known_default() == b.port_or_known_default()
        }
        _ => false,
    }
}

/// `structuredClone(value)` through the bootstrap's pristine implementation —
/// `undefined` clones to `null`, which is what `history.state` reads back as.
fn structured_clone(cx: &BindCx<'_>, value: JsValue) -> Result<JsValue, JsThrow> {
    if value.is_undefined() {
        return Ok(JsValue::Null);
    }
    let clone = cx.with_js(|js| js.structured_clone.clone())?;
    cx.scope
        .call(&clone, &JsValue::Undefined, &[value])
        .map_err(JsThrow::from)
}
