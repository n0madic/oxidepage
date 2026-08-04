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
    Ok(cx.state.page.history.borrow().len() as f64)
}

pub(crate) fn scroll_restoration(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    Ok(cx
        .state
        .page
        .history
        .borrow()
        .scroll_restoration()
        .to_owned())
}

/// Stored and reflected, nothing more: there is no bfcache whose scroll
/// position could be restored, and the value is observable, so recording it is
/// honest where silently discarding it would not be.
pub(crate) fn set_scroll_restoration(
    cx: &BindCx<'_>,
    _this: u64,
    value: String,
) -> Result<(), JsThrow> {
    cx.state
        .page
        .history
        .borrow_mut()
        .set_scroll_restoration(&value);
    Ok(())
}

pub(crate) fn state(cx: &BindCx<'_>, _this: u64) -> Result<JsValue, JsThrow> {
    let serialized = cx
        .state
        .page
        .history
        .borrow()
        .current()
        .and_then(|entry| entry.state.clone());
    deserialize_state(cx, serialized.as_deref())
}

/// Materializes a serialized history state **in this world**.
///
/// The value is cached per world so `history.state === history.state` holds
/// within a world, which is what script that stashes it expects; across worlds
/// the objects are necessarily distinct, since no value can cross (ADR-0033 D5).
pub(crate) fn deserialize_state(
    cx: &BindCx<'_>,
    serialized: Option<&str>,
) -> Result<JsValue, JsThrow> {
    let Some(text) = serialized else {
        return Ok(JsValue::Null);
    };
    if let Some((cached_text, value)) = cx.state.history_state_cache.borrow().as_ref()
        && cached_text == text
    {
        return Ok(value.clone());
    }
    let parse = cx.with_js(|js| js.json_parse.clone())?;
    let value = cx
        .scope
        .call(
            &parse,
            &JsValue::Undefined,
            &[JsValue::String(text.to_owned())],
        )
        .map_err(JsThrow::from)?;
    *cx.state.history_state_cache.borrow_mut() = Some((text.to_owned(), value.clone()));
    Ok(value)
}

/// Serializes an event's state value, for `PopStateEvent`'s constructor.
pub(crate) fn serialize_for_event(
    cx: &BindCx<'_>,
    value: &JsValue,
) -> Result<Option<String>, JsThrow> {
    serialize_state(cx, value)
}

/// Serializes a `pushState`/`replaceState` value for page-level storage.
///
/// `null`/`undefined` become `None`. A value JSON cannot represent (a cycle)
/// is stored as `None` rather than throwing: `structuredClone` above has
/// already enforced the spec's `DataCloneError` cases, and this round trip is
/// storage, not validation.
fn serialize_state(cx: &BindCx<'_>, value: &JsValue) -> Result<Option<String>, JsThrow> {
    if value.is_undefined() || matches!(value, JsValue::Null) {
        return Ok(None);
    }
    let stringify = cx.with_js(|js| js.json_stringify.clone())?;
    let text = cx
        .scope
        .call(&stringify, &JsValue::Undefined, std::slice::from_ref(value))
        .map_err(JsThrow::from)?;
    Ok(match text {
        JsValue::String(text) => Some(text),
        _ => None,
    })
}

/// `history.go(delta)`. `0` means "reload the current entry" — the one case
/// that is a real navigation even though the index does not move.
pub(crate) fn go(cx: &BindCx<'_>, _this: u64, delta: i32) -> Result<(), JsThrow> {
    if delta == 0 {
        return crate::imp::location::reload(cx, 0);
    }
    // Out of range is a silent no-op, per HTML's "traverse the history by a
    // delta": if there is no such entry, return.
    if cx.state.page.history.borrow().target_of(delta).is_none() {
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
    // Stored as text so every world can materialize its own copy (D3/D5). The
    // structured clone above still runs, and still throws, because it is what
    // enforces the spec's `DataCloneError` — JSON would silently accept a DOM
    // node. The cost is that a `Date` or `Map` in the state comes back as its
    // JSON shape; recorded as a deliberate limit.
    let serialized = serialize_state(cx, &cloned)?;

    cx.state.dom.borrow_mut().set_document_url(target.clone());
    let mut history = cx.state.page.history.borrow_mut();
    let seq = history.document_seq();
    if replace {
        history.replace(target, serialized, seq);
    } else {
        history.push(target, serialized, seq);
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
