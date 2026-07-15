//! `HTMLLabelElement`.

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::imp::reflect::{reflect_string, set_string};

/// `htmlFor` reflects the `for` content attribute — the IDL name differs from
/// the attribute name (`for` is a reserved word), so no macro.
pub(crate) fn html_for(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(reflect_string(cx, this, "for"))
}

pub(crate) fn set_html_for(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    set_string(cx, this, "for", value);
    Ok(())
}

/// The labelled control: the element named by `for`, else the first labelable
/// descendant.
pub(crate) fn control(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(cx.state.dom.borrow().label_control(this))
}

/// A label's form owner is its control's.
pub(crate) fn form(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(dom.label_control(this).and_then(|c| dom.form_owner(c)))
}
