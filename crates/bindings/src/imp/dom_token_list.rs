//! `DOMTokenList` implementation.

use html5ever::local_name;
use oxidepage_base::DomExceptionKind;
use oxidepage_dom::LocalName;
use oxidepage_dom::node::attr_name;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::imp::{token_list_parts, token_list_tokens};

fn validate_token(cx: &BindCx<'_>, token: &str) -> Result<(), JsThrow> {
    if token.is_empty() {
        return Err(cx.dom_throw(DomExceptionKind::SyntaxError, "token must not be empty"));
    }
    if token.chars().any(|c| c.is_ascii_whitespace()) {
        return Err(cx.dom_throw(
            DomExceptionKind::InvalidCharacterError,
            "token must not contain whitespace",
        ));
    }
    Ok(())
}

/// Writes the token list back to its backing attribute.
fn update(cx: &BindCx<'_>, this: u64, tokens: &[String]) -> Result<(), JsThrow> {
    let (element, attr) = token_list_parts(cx, this)?;
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(element, attr_name(attr), tokens.join(" ").into());
    Ok(())
}

pub(crate) fn length(cx: &BindCx<'_>, this: u64) -> Result<f64, JsThrow> {
    Ok(token_list_tokens(cx, this).len() as f64)
}

pub(crate) fn item(cx: &BindCx<'_>, this: u64, index: u32) -> Result<Option<String>, JsThrow> {
    Ok(token_list_tokens(cx, this).get(index as usize).cloned())
}

pub(crate) fn contains(cx: &BindCx<'_>, this: u64, token: String) -> Result<bool, JsThrow> {
    Ok(token_list_tokens(cx, this).contains(&token))
}

pub(crate) fn add(cx: &BindCx<'_>, this: u64, tokens: Vec<String>) -> Result<(), JsThrow> {
    for token in &tokens {
        validate_token(cx, token)?;
    }
    let mut current = token_list_tokens(cx, this);
    for token in tokens {
        if !current.contains(&token) {
            current.push(token);
        }
    }
    update(cx, this, &current)
}

pub(crate) fn remove(cx: &BindCx<'_>, this: u64, tokens: Vec<String>) -> Result<(), JsThrow> {
    for token in &tokens {
        validate_token(cx, token)?;
    }
    let mut current = token_list_tokens(cx, this);
    current.retain(|t| !tokens.contains(t));
    update(cx, this, &current)
}

pub(crate) fn toggle(
    cx: &BindCx<'_>,
    this: u64,
    token: String,
    force: Option<bool>,
) -> Result<bool, JsThrow> {
    validate_token(cx, &token)?;
    let current = token_list_tokens(cx, this);
    let has = current.contains(&token);
    match (has, force) {
        (true, None | Some(false)) => {
            remove(cx, this, vec![token])?;
            Ok(false)
        }
        (true, Some(true)) => Ok(true),
        (false, None | Some(true)) => {
            add(cx, this, vec![token])?;
            Ok(true)
        }
        (false, Some(false)) => Ok(false),
    }
}

pub(crate) fn replace(
    cx: &BindCx<'_>,
    this: u64,
    token: String,
    new_token: String,
) -> Result<bool, JsThrow> {
    validate_token(cx, &token)?;
    validate_token(cx, &new_token)?;
    let mut current = token_list_tokens(cx, this);
    let Some(position) = current.iter().position(|t| *t == token) else {
        return Ok(false);
    };
    // Spec: replace the first occurrence, drop other occurrences of both.
    current[position] = new_token.clone();
    let mut seen = Vec::new();
    for t in current {
        if !seen.contains(&t) {
            seen.push(t);
        }
    }
    update(cx, this, &seen)?;
    Ok(true)
}

/// The link types the engine actually acts on, per element that has a
/// `relList`. The HTML spec defines `rel`'s supported tokens as the keywords
/// "supported by the user agent", so this stays honest (P6): `<link
/// rel=stylesheet>` is fetched and applied, while the preload/prefetch hints
/// and the navigation-only `noopener`/`noreferrer` keywords are not
/// implemented and report as unsupported rather than silently doing nothing.
///
/// `rel` on `<a>`/`<area>` still *defines* supported tokens (an empty set
/// here), which is what keeps `supports()` from throwing on those elements.
fn supported_rel_tokens(tag: &LocalName) -> &'static [&'static str] {
    if *tag == local_name!("link") {
        &["stylesheet"]
    } else {
        &[]
    }
}

pub(crate) fn supports(cx: &BindCx<'_>, this: u64, token: String) -> Result<bool, JsThrow> {
    let (element, attr) = token_list_parts(cx, this)?;
    // `class` and `part` define no supported tokens, so they must throw. Only
    // `rel` answers the query (spec: DOMTokenList validation steps).
    if &*attr != "rel" {
        return Err(JsThrow::Type("DOMTokenList has no supported tokens".into()));
    }
    let dom = cx.state.dom.borrow();
    let Some(tag) = dom
        .get(element)
        .and_then(|node| node.as_element())
        .map(|el| el.name.local.clone())
    else {
        return Ok(false);
    };
    // Spec: compare the ASCII-lowercased token; unlike `add`/`remove`, an
    // empty or whitespace-bearing token is simply unsupported, not an error.
    let token = token.to_ascii_lowercase();
    Ok(supported_rel_tokens(&tag).contains(&token.as_str()))
}

pub(crate) fn value(cx: &BindCx<'_>, this: u64) -> Result<String, JsThrow> {
    let (element, attr) = token_list_parts(cx, this)?;
    let dom = cx.state.dom.borrow();
    Ok(dom
        .node(element)
        .as_element()
        .and_then(|el| el.attr(&attr_name(attr)))
        .map(|v| v.to_string())
        .unwrap_or_default())
}

pub(crate) fn set_value(cx: &BindCx<'_>, this: u64, value: String) -> Result<(), JsThrow> {
    let (element, attr) = token_list_parts(cx, this)?;
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(element, attr_name(attr), value.into());
    Ok(())
}
