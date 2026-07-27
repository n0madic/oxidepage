//! Members shared by the form-control interfaces.
//!
//! `form`, `labels`, and the value/checkedness pairs have one definition each
//! and are re-exported by the per-tag modules. `<input>` and `<textarea>` differ
//! in *where* their default value lives (a content attribute vs. the child
//! text), and [`oxidepage_dom::form`] already resolves that, so even `value` is
//! one function here.

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;

/// The control's form owner (`input.form`, `select.form`, …).
pub(crate) fn form(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(cx.state.dom.borrow().form_owner(this))
}

/// The `<label>`s labelling this control.
pub(crate) fn labels(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "labels", |cx| {
        cx.new_collection("NodeList", CollectionData::Labels(this))
    })
}

/// `value` — the form state's value, which the dirty value flag decouples from
/// the content attribute.
pub(crate) fn value(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(cx.state.dom.borrow().form_value(this))
}

pub(crate) fn set_value(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    cx.state.dom.borrow_mut().set_form_value(this, value);
    Ok(())
}

/// `defaultValue` — the reflecting half of the pair.
pub(crate) fn default_value(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(cx.state.dom.borrow().form_default_value(this))
}

// --- The submit-button overrides, shared by `<button>` and `<input>` ---------
//
// These four are what let a single form carry several destinations, so they
// have to be readable as well as effective: `imp::form_submit` consults the
// content attributes, and these are the script-visible half.

/// `formAction`. Reflects `formaction`, except that a missing or empty
/// attribute reads back as the document's URL — which is where the submission
/// would in fact go.
pub(crate) fn form_action(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let raw = super::reflect::reflect_string(cx, this, "formaction");
    if raw.trim().is_empty() {
        return Ok(cx.state.dom.borrow().document_url().to_owned());
    }
    Ok(super::reflect::reflect_url(cx, this, "formaction"))
}

pub(crate) fn set_form_action(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    super::reflect::set_string(cx, this, "formaction", value);
    Ok(())
}

/// `formEnctype`, reflected *limited to only known values*: an unrecognised
/// value reads back as the invalid-value default, a missing one as `""` (the
/// form's own `enctype` applies).
pub(crate) fn form_enctype(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(limited_to_known(
        cx,
        this,
        "formenctype",
        &[
            "application/x-www-form-urlencoded",
            "multipart/form-data",
            "text/plain",
        ],
        "application/x-www-form-urlencoded",
    ))
}

pub(crate) fn set_form_enctype(
    cx: &BindCx<'_>,
    this: NodeId,
    value: String,
) -> Result<(), JsThrow> {
    super::reflect::set_string(cx, this, "formenctype", value);
    Ok(())
}

/// `formMethod`, reflected limited to only known values (invalid-value default
/// `get`, missing-value default `""`).
pub(crate) fn form_method(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(limited_to_known(
        cx,
        this,
        "formmethod",
        &["get", "post", "dialog"],
        "get",
    ))
}

pub(crate) fn set_form_method(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    super::reflect::set_string(cx, this, "formmethod", value);
    Ok(())
}

pub(crate) fn form_no_validate(cx: &BindCx<'_>, this: NodeId) -> Result<bool, JsThrow> {
    Ok(super::reflect::reflect_bool(cx, this, "formnovalidate"))
}

pub(crate) fn set_form_no_validate(
    cx: &BindCx<'_>,
    this: NodeId,
    value: bool,
) -> Result<(), JsThrow> {
    super::reflect::set_bool(cx, this, "formnovalidate", value);
    Ok(())
}

/// The reflection rule for an enumerated attribute "limited to only known
/// values": a keyword reads back lowercased, anything else reads back as
/// `invalid_default`, and a missing attribute reads back as `""`.
fn limited_to_known(
    cx: &BindCx<'_>,
    this: NodeId,
    attr: &str,
    keywords: &[&str],
    invalid_default: &str,
) -> String {
    let raw = super::reflect::reflect_string(cx, this, attr);
    if raw.is_empty() {
        return String::new();
    }
    let lower = raw.to_ascii_lowercase();
    if keywords.contains(&lower.as_str()) {
        lower
    } else {
        invalid_default.to_owned()
    }
}
