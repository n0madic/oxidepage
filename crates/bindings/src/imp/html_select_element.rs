//! `HTMLSelectElement`.
//!
//! A select has no value of its own: `value`, `selectedIndex` and `length` are
//! all views over its option list, so they all delegate to the DOM, which owns
//! the selectedness invariants (including "ask for a reset").

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;
use crate::imp::reflect::{bool_reflector, string_reflector, u32_reflector};

pub(crate) use crate::imp::form_support::{form, labels};

string_reflector!(name, set_name, "name");
bool_reflector!(disabled, set_disabled, "disabled");
bool_reflector!(required, set_required, "required");
bool_reflector!(multiple, set_multiple, "multiple");
u32_reflector!(size, set_size, "size");

pub(crate) fn value(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(cx.state.dom.borrow().select_value(this))
}

pub(crate) fn set_value(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    cx.state.dom.borrow_mut().set_select_value(this, &value);
    Ok(())
}

pub(crate) fn selected_index(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(f64::from(cx.state.dom.borrow().select_selected_index(this)))
}

pub(crate) fn set_selected_index(cx: &BindCx<'_>, this: NodeId, value: i32) -> Result<(), JsThrow> {
    cx.state
        .dom
        .borrow_mut()
        .set_select_selected_index(this, value);
    Ok(())
}

pub(crate) fn length(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(cx.state.dom.borrow().select_options(this).len() as f64)
}

/// `select.type` distinguishes the two selection modes — jQuery's `val()` hook
/// branches on exactly this string.
pub(crate) fn r#type(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let multiple = crate::imp::reflect::reflect_bool(cx, this, "multiple");
    Ok(if multiple {
        "select-multiple".to_owned()
    } else {
        "select-one".to_owned()
    })
}

pub(crate) fn options(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "options", |cx| {
        cx.new_collection("HTMLCollection", CollectionData::SelectOptions(this))
    })
}

pub(crate) fn selected_options(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "selectedOptions", |cx| {
        cx.new_collection("HTMLCollection", CollectionData::SelectedOptions(this))
    })
}
