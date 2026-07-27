//! `HTMLTextAreaElement`.
//!
//! A textarea has no `value` content attribute: its *default* value is its child
//! text, so `defaultValue` reads and writes the text content while `value` is
//! form state. [`oxidepage_dom::DomTree::form_value`] already knows the
//! difference, so `value` comes from the shared helper unchanged.

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::imp::reflect::{bool_reflector, string_reflector, u32_reflector};

pub(crate) use crate::imp::form_support::{default_value, form, labels, set_value, value};

string_reflector!(name, set_name, "name");
string_reflector!(placeholder, set_placeholder, "placeholder");
bool_reflector!(disabled, set_disabled, "disabled");
bool_reflector!(read_only, set_read_only, "readonly");
bool_reflector!(required, set_required, "required");
u32_reflector!(rows, set_rows, "rows");
u32_reflector!(cols, set_cols, "cols");

pub(crate) fn set_default_value(
    cx: &BindCx<'_>,
    this: NodeId,
    value: String,
) -> Result<(), JsThrow> {
    super::node::set_text_content(cx, this, Some(value))
}

/// `textarea.type` is the constant `"textarea"`.
pub(crate) fn r#type(_cx: &BindCx<'_>, _this: NodeId) -> Result<String, JsThrow> {
    Ok("textarea".to_owned())
}

pub(crate) fn text_length(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    // UTF-16 code units, as every DOM string length is.
    let len = cx
        .state
        .dom
        .borrow()
        .form_value(this)
        .encode_utf16()
        .count();
    Ok(len as f64)
}

// === Text selection (shared with `HTMLTextAreaElement`; see `imp::text_selection`) ===

pub(crate) use crate::imp::text_selection::{
    max_length, min_length, select, selection_direction, selection_end, selection_start,
    set_max_length, set_min_length, set_selection_direction, set_selection_end,
    set_selection_range, set_selection_start,
};
