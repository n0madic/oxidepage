//! `Location`: the document URL, readable component-wise and writable as a
//! navigation.
//!
//! A Location holds no state of its own — `this` is a brand token, and every
//! getter reads `DomTree::document_url()`. That is deliberate: the document URL
//! is one value with one owner, and a `<base href>` never moves it.
//!
//! Every setter *navigates*. It cannot navigate inline — the call runs under
//! live `RefCell` borrows on the DOM/style/layout engines, and committing a
//! document replaces all three — so it queues a [`PendingNavigation`] that the
//! page's event loop performs (ADR-0022). A fragment-only write comes out of
//! here indistinguishable from any other load; the page's classifier is what
//! turns it into a same-document navigation.
//!
//! Cross-origin writes are allowed. Navigating away from the current origin is
//! what a Location is *for*; the same-origin restriction belongs to
//! `History.pushState`, which changes the URL without loading.

use oxidepage_base::DomExceptionKind;
use oxidepage_js::JsThrow;
use url::Url;

use crate::cx::BindCx;
use crate::imp::url_parts as parts;
use crate::state::PendingNavigation;

/// The document URL as a string. Also `toString`, via the IDL stringifier.
fn current_href(cx: &BindCx<'_>) -> String {
    cx.state.dom.borrow().document_url().to_owned()
}

/// The document URL, parsed. `None` for an opaque or unparseable document URL
/// (`about:blank` predecessors, embedder-supplied strings), which the whole
/// decomposition surface reports as `""` — a browser's opaque-origin Location.
fn current(cx: &BindCx<'_>) -> Option<Url> {
    Url::parse(&current_href(cx)).ok()
}

fn part(cx: &BindCx<'_>, f: impl FnOnce(&Url) -> String) -> Result<String, JsThrow> {
    Ok(current(cx).map(|url| f(&url)).unwrap_or_default())
}

/// Queues a navigation to `url`. `replace` overwrites the current session
/// history entry instead of pushing one.
fn navigate_to(cx: &BindCx<'_>, url: String, replace: bool) {
    cx.state.request_navigation(PendingNavigation::Load {
        url,
        replace,
        body: None,
        reload: false,
    });
}

/// Applies `f` to the parsed document URL and navigates to the result. An
/// unparseable document URL leaves nothing to modify, so the setter is a
/// silent no-op — the same "on failure, do nothing" the URL standard gives the
/// decomposition setters.
fn update(cx: &BindCx<'_>, f: impl FnOnce(&mut Url)) -> Result<(), JsThrow> {
    if let Some(mut url) = current(cx) {
        f(&mut url);
        navigate_to(cx, url.to_string(), false);
    }
    Ok(())
}

pub(crate) fn href(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    Ok(current_href(cx))
}

/// `location.href = "..."` — resolved against the document URL, exactly like
/// `assign()`.
pub(crate) fn set_href(cx: &BindCx<'_>, this: u64, value: String) -> Result<(), JsThrow> {
    assign(cx, this, value)
}

pub(crate) fn origin(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    part(cx, parts::origin)
}

pub(crate) fn protocol(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    part(cx, parts::protocol)
}

pub(crate) fn set_protocol(cx: &BindCx<'_>, _this: u64, value: String) -> Result<(), JsThrow> {
    update(cx, |url| parts::set_protocol(url, &value))
}

pub(crate) fn host(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    part(cx, parts::host)
}

pub(crate) fn set_host(cx: &BindCx<'_>, _this: u64, value: String) -> Result<(), JsThrow> {
    update(cx, |url| parts::set_host(url, &value))
}

pub(crate) fn hostname(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    part(cx, parts::hostname)
}

pub(crate) fn set_hostname(cx: &BindCx<'_>, _this: u64, value: String) -> Result<(), JsThrow> {
    update(cx, |url| parts::set_hostname(url, &value))
}

pub(crate) fn port(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    part(cx, parts::port)
}

pub(crate) fn set_port(cx: &BindCx<'_>, _this: u64, value: String) -> Result<(), JsThrow> {
    update(cx, |url| parts::set_port(url, &value))
}

pub(crate) fn pathname(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    part(cx, parts::pathname)
}

pub(crate) fn set_pathname(cx: &BindCx<'_>, _this: u64, value: String) -> Result<(), JsThrow> {
    update(cx, |url| parts::set_pathname(url, &value))
}

pub(crate) fn search(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    part(cx, parts::search)
}

pub(crate) fn set_search(cx: &BindCx<'_>, _this: u64, value: String) -> Result<(), JsThrow> {
    update(cx, |url| parts::set_search(url, &value))
}

pub(crate) fn hash(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    part(cx, parts::hash)
}

/// `location.hash = "#x"`. Queued as an ordinary load whose URL differs from
/// the current one only in the fragment; the page's classifier recognises that
/// and performs a same-document navigation (`hashchange`, no request).
///
/// **Not** `url_parts::set_hash`, which is the *URL* standard's setter and maps
/// `""` to a null fragment. HTML's Location setter always leaves a fragment —
/// the empty one for `""` — and that difference is load-bearing here: a null
/// fragment would make `location.hash = ""` fail the fragment test and reload
/// the document instead of scrolling to the top of it.
///
/// It also returns early when the fragment is unchanged, so re-assigning the
/// same hash is not a navigation at all (and grows neither the history nor the
/// `hashchange` stream).
pub(crate) fn set_hash(cx: &BindCx<'_>, _this: u64, value: String) -> Result<(), JsThrow> {
    let Some(mut url) = current(cx) else {
        return Ok(());
    };
    let fragment = value.strip_prefix('#').unwrap_or(&value);
    if url.fragment().unwrap_or_default() == fragment {
        return Ok(());
    }
    url.set_fragment(Some(fragment));
    navigate_to(cx, url.to_string(), false);
    Ok(())
}

/// `location.assign(url)`: navigate, pushing a session-history entry.
pub(crate) fn assign(cx: &BindCx<'_>, _this: u64, url: String) -> Result<(), JsThrow> {
    let resolved = resolve(cx, &url)?;
    navigate_to(cx, resolved, false);
    Ok(())
}

/// `location.replace(url)`: navigate *without* growing the session history.
pub(crate) fn replace(cx: &BindCx<'_>, _this: u64, url: String) -> Result<(), JsThrow> {
    let resolved = resolve(cx, &url)?;
    navigate_to(cx, resolved, true);
    Ok(())
}

/// `location.reload()`: re-fetch the current URL, bypassing the HTTP cache and
/// replacing the current entry rather than pushing a duplicate.
pub(crate) fn reload(cx: &BindCx<'_>, _this: u64) -> Result<(), JsThrow> {
    cx.state.request_navigation(PendingNavigation::Load {
        url: current_href(cx),
        replace: true,
        body: None,
        reload: true,
    });
    Ok(())
}

/// Resolves `url` against the document URL. A value that will not parse is a
/// `SyntaxError`, as HTML's "location-object navigate" specifies.
fn resolve(cx: &BindCx<'_>, url: &str) -> Result<String, JsThrow> {
    let base = current_href(cx);
    let joined = Url::parse(&base)
        .and_then(|base| base.join(url))
        .or_else(|_| Url::parse(url));
    joined.map(|u| u.to_string()).map_err(|_| {
        cx.dom_throw(
            DomExceptionKind::SyntaxError,
            &format!("Location: `{url}` is not a valid URL"),
        )
    })
}
