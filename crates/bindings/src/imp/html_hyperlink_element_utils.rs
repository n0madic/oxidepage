//! `HTMLHyperlinkElementUtils`: the `href` + URL-decomposition surface shared by
//! `HTMLAnchorElement` and `HTMLAreaElement`.
//!
//! Every getter works off the element's *resolved* href — the `href` content
//! attribute joined onto the document base URL. When there is no href, or it
//! will not parse, the spec makes the whole decomposition return `""` and makes
//! the setters no-ops; that is the `None` branch of [`resolved`] throughout.

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;
use url::Url;

use crate::cx::BindCx;
use crate::imp::reflect::{reflect_url, set_string};
use crate::imp::url_parts as parts;

/// The element's href, parsed. `None` when absent or unparseable.
fn resolved(cx: &BindCx<'_>, this: NodeId) -> Option<Url> {
    Url::parse(&reflect_url(cx, this, "href")).ok()
}

/// Reads the resolved href, applies `f`, and writes the result back to the
/// `href` content attribute. Without a parseable href there is nothing to
/// modify, so the setter silently does nothing (per spec).
fn update(cx: &BindCx<'_>, this: NodeId, f: impl FnOnce(&mut Url)) -> Result<(), JsThrow> {
    if let Some(mut url) = resolved(cx, this) {
        f(&mut url);
        set_string(cx, this, "href", url.to_string());
    }
    Ok(())
}

/// Reads the resolved href and serializes one component of it, or `""`.
fn part(cx: &BindCx<'_>, this: NodeId, f: impl FnOnce(&Url) -> String) -> Result<String, JsThrow> {
    Ok(resolved(cx, this).map(|url| f(&url)).unwrap_or_default())
}

pub(crate) fn href(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(reflect_url(cx, this, "href"))
}

pub(crate) fn set_href(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    set_string(cx, this, "href", value);
    Ok(())
}

pub(crate) fn origin(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    part(cx, this, parts::origin)
}

pub(crate) fn protocol(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    part(cx, this, parts::protocol)
}

pub(crate) fn set_protocol(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    update(cx, this, |url| parts::set_protocol(url, &value))
}

pub(crate) fn username(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    part(cx, this, parts::username)
}

pub(crate) fn set_username(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    update(cx, this, |url| parts::set_username(url, &value))
}

pub(crate) fn password(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    part(cx, this, parts::password)
}

pub(crate) fn set_password(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    update(cx, this, |url| parts::set_password(url, &value))
}

pub(crate) fn host(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    part(cx, this, parts::host)
}

pub(crate) fn set_host(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    update(cx, this, |url| parts::set_host(url, &value))
}

pub(crate) fn hostname(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    part(cx, this, parts::hostname)
}

pub(crate) fn set_hostname(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    update(cx, this, |url| parts::set_hostname(url, &value))
}

pub(crate) fn port(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    part(cx, this, parts::port)
}

pub(crate) fn set_port(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    update(cx, this, |url| parts::set_port(url, &value))
}

pub(crate) fn pathname(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    part(cx, this, parts::pathname)
}

pub(crate) fn set_pathname(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    update(cx, this, |url| parts::set_pathname(url, &value))
}

pub(crate) fn search(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    part(cx, this, parts::search)
}

pub(crate) fn set_search(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    update(cx, this, |url| parts::set_search(url, &value))
}

pub(crate) fn hash(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    part(cx, this, parts::hash)
}

pub(crate) fn set_hash(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    update(cx, this, |url| parts::set_hash(url, &value))
}
