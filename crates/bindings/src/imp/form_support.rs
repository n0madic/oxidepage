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
