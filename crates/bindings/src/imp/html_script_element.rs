//! Practical `HTMLScriptElement` reflection.

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::imp::reflect::{
    bool_reflector, nullable_string_reflector, reflect_bool, set_bool, string_reflector,
    url_reflector,
};

string_reflector!(r#type, set_type, "type");
bool_reflector!(defer, set_defer, "defer");
bool_reflector!(no_module, set_no_module, "nomodule");
url_reflector!(src, set_src, "src");
nullable_string_reflector!(cross_origin, set_cross_origin, "crossorigin");

pub(crate) fn r#async(cx: &BindCx<'_>, this: NodeId) -> Result<bool, JsThrow> {
    Ok(cx.state.dom.borrow().script_force_async(this) || reflect_bool(cx, this, "async"))
}

pub(crate) fn set_async(cx: &BindCx<'_>, this: NodeId, value: bool) -> Result<(), JsThrow> {
    cx.state
        .dom
        .borrow_mut()
        .set_script_force_async(this, false);
    set_bool(cx, this, "async", value);
    Ok(())
}

pub(crate) fn text(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(cx.state.dom.borrow().text_content(this))
}

pub(crate) fn set_text(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    super::node::set_text_content(cx, this, Some(value))
}

// `onload`/`onerror` are not declared here: they are `GlobalEventHandlers`
// members, which `HTMLElement` includes, so a script element inherits them from
// `HTMLElement.prototype` — which is also where browsers put them.
