//! `HTMLInputElement`.
//!
//! `value`/`checked`/`indeterminate` are form *state* (see
//! [`oxidepage_dom::form`]); everything else reflects a content attribute.
//! `defaultValue`/`defaultChecked` are the reflecting halves of the first two.

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::imp::reflect::{bool_reflector, set_string, string_reflector};

pub(crate) use crate::imp::form_support::{
    default_value, form, form_action, form_enctype, form_method, form_no_validate, labels,
    set_form_action, set_form_enctype, set_form_method, set_form_no_validate, set_value, value,
};

string_reflector!(name, set_name, "name");
string_reflector!(placeholder, set_placeholder, "placeholder");
bool_reflector!(disabled, set_disabled, "disabled");
bool_reflector!(read_only, set_read_only, "readonly");
bool_reflector!(required, set_required, "required");
bool_reflector!(multiple, set_multiple, "multiple");
bool_reflector!(default_checked, set_default_checked, "checked");

/// `input.type` is a *limited-to-known-values* reflection: an unknown or missing
/// `type` reads back as `"text"`, the invalid value default.
pub(crate) fn r#type(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(dom
        .node(this)
        .as_element()
        .map(oxidepage_dom::input_type)
        .unwrap_or("text")
        .to_owned())
}

pub(crate) fn set_type(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    set_string(cx, this, "type", value);
    Ok(())
}

/// The setter writes the `value` content attribute — `defaultValue` is the one
/// member of the pair that still reflects.
pub(crate) fn set_default_value(
    cx: &BindCx<'_>,
    this: NodeId,
    value: String,
) -> Result<(), JsThrow> {
    set_string(cx, this, "value", value);
    Ok(())
}

pub(crate) fn checked(cx: &BindCx<'_>, this: NodeId) -> Result<bool, JsThrow> {
    Ok(cx.state.dom.borrow().checkedness(this))
}

pub(crate) fn set_checked(cx: &BindCx<'_>, this: NodeId, value: bool) -> Result<(), JsThrow> {
    cx.state.dom.borrow_mut().set_checkedness(this, value);
    Ok(())
}

pub(crate) fn indeterminate(cx: &BindCx<'_>, this: NodeId) -> Result<bool, JsThrow> {
    Ok(cx.state.dom.borrow().indeterminate(this))
}

pub(crate) fn set_indeterminate(cx: &BindCx<'_>, this: NodeId, value: bool) -> Result<(), JsThrow> {
    cx.state.dom.borrow_mut().set_indeterminate(this, value);
    Ok(())
}

// === Text selection (shared with `HTMLTextAreaElement`; see `imp::text_selection`) ===

pub(crate) use crate::imp::text_selection::{
    max_length, min_length, select, selection_direction, selection_end, selection_start,
    set_max_length, set_min_length, set_selection_direction, set_selection_end,
    set_selection_range, set_selection_start,
};
