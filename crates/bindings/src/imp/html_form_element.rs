//! `HTMLFormElement`.
//!
//! Known simplification: the real `method` and `enctype` getters canonicalize
//! to their enumerated keywords (`"get"`, `"application/x-www-form-urlencoded"`,
//! …) and `action` falls back to the document URL when the attribute is empty.
//! We reflect the raw attribute in all three cases.
//!
//! `elements` is a plain `HTMLCollection`, not an `HTMLFormControlsCollection`
//! (that interface adds only the `namedItem` overload returning a RadioNodeList).
//!
//! `submit()`/`requestSubmit()` live in [`crate::imp::form_submit`], which owns
//! the whole submission algorithm; the two differ only in whether the `submit`
//! event fires, exactly as HTML specifies.

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;
use crate::imp::reflect::{
    bool_reflector, reflect_string, set_string, string_reflector, url_reflector,
};

url_reflector!(action, set_action, "action");
string_reflector!(method, set_method, "method");
string_reflector!(enctype, set_enctype, "enctype");
string_reflector!(target, set_target, "target");
string_reflector!(name, set_name, "name");
bool_reflector!(no_validate, set_no_validate, "novalidate");

// The IDL name (`acceptCharset`) and the content attribute name
// (`accept-charset`) differ, so this pair cannot come from the macro.
pub(crate) fn accept_charset(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(reflect_string(cx, this, "accept-charset"))
}

pub(crate) fn set_accept_charset(
    cx: &BindCx<'_>,
    this: NodeId,
    value: String,
) -> Result<(), JsThrow> {
    set_string(cx, this, "accept-charset", value);
    Ok(())
}

pub(crate) fn elements(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "elements", |cx| {
        cx.new_collection("HTMLCollection", CollectionData::FormControls(this))
    })
}

pub(crate) fn length(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(cx.state.dom.borrow().form_controls(this).len() as f64)
}

/// HTML "reset the form owner": clear every control's dirty flags, so each one
/// falls back to its content attribute again.
///
/// `form.reset()` does *not* fire the `reset` event — only the reset button's
/// activation behavior does (`imp::form_submit::reset`).
pub(crate) fn reset(cx: &BindCx<'_>, this: NodeId) -> Result<(), JsThrow> {
    cx.state.dom.borrow_mut().reset_form(this);
    Ok(())
}

/// `form.submit()`: submit **without** firing `submit` and without validating,
/// per HTML.
pub(crate) fn submit(cx: &BindCx<'_>, this: NodeId) -> Result<(), JsThrow> {
    crate::imp::form_submit::submit(cx, this, None, /* fire_event */ false)
}

/// `form.requestSubmit(submitter?)`: what a click on a submit button does.
pub(crate) fn request_submit(
    cx: &BindCx<'_>,
    this: NodeId,
    submitter: Option<NodeId>,
) -> Result<(), JsThrow> {
    crate::imp::form_submit::request_submit(cx, this, submitter)
}
