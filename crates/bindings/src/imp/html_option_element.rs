//! `HTMLOptionElement`.
//!
//! `selected` is form state (the dirty *selectedness* flag); `defaultSelected`
//! reflects the `selected` content attribute. Setting `selected` in a
//! single-selection `<select>` must deselect the siblings, which the DOM does.

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::imp::reflect::{bool_reflector, set_string, string_reflector};

pub(crate) use crate::imp::form_support::form;

bool_reflector!(disabled, set_disabled, "disabled");
string_reflector!(label, set_label, "label");
bool_reflector!(default_selected, set_default_selected, "selected");

/// `option.value` falls back to the option's text when the attribute is absent
/// — handled in the DOM, so this is the shared getter.
pub(crate) use crate::imp::form_support::value;

pub(crate) fn set_value(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    // Unlike `<input>`, `option.value` is a plain reflection of the content
    // attribute — there is no dirty value flag on an option.
    set_string(cx, this, "value", value);
    Ok(())
}

pub(crate) fn text(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(cx.state.dom.borrow().option_text(this))
}

pub(crate) fn set_text(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    super::node::set_text_content(cx, this, Some(value))
}

pub(crate) fn selected(cx: &BindCx<'_>, this: NodeId) -> Result<bool, JsThrow> {
    Ok(cx.state.dom.borrow().checkedness(this))
}

pub(crate) fn set_selected(cx: &BindCx<'_>, this: NodeId, value: bool) -> Result<(), JsThrow> {
    cx.state.dom.borrow_mut().set_option_selected(this, value);
    Ok(())
}

/// The option's index in its `<select>`'s option list, or 0 when it has none.
pub(crate) fn index(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    let dom = cx.state.dom.borrow();
    let Some(select) = dom.owner_select(this) else {
        return Ok(0.0);
    };
    let index = dom
        .select_options(select)
        .iter()
        .position(|&o| o == this)
        .unwrap_or(0);
    Ok(index as f64)
}
