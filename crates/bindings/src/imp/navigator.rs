//! Practical, immutable `Navigator` identity and capability surface.

use std::rc::Rc;

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::NavigatorData;

macro_rules! string_getter {
    ($name:ident, $field:ident) => {
        pub(crate) fn $name(_cx: &BindCx<'_>, this: Rc<NavigatorData>) -> Result<String, JsThrow> {
            Ok(this.$field.clone())
        }
    };
}

pub(crate) fn app_code_name(_cx: &BindCx<'_>, _this: Rc<NavigatorData>) -> Result<String, JsThrow> {
    Ok("Mozilla".to_owned())
}

pub(crate) fn app_name(_cx: &BindCx<'_>, _this: Rc<NavigatorData>) -> Result<String, JsThrow> {
    Ok("Netscape".to_owned())
}

pub(crate) fn app_version(_cx: &BindCx<'_>, this: Rc<NavigatorData>) -> Result<String, JsThrow> {
    if this.user_agent.starts_with("Mozilla/5.0 (") {
        Ok(this.user_agent["Mozilla/".len()..].to_owned())
    } else {
        Ok(String::new())
    }
}

string_getter!(platform, platform);

pub(crate) fn product(_cx: &BindCx<'_>, _this: Rc<NavigatorData>) -> Result<String, JsThrow> {
    Ok("Gecko".to_owned())
}

pub(crate) fn product_sub(_cx: &BindCx<'_>, _this: Rc<NavigatorData>) -> Result<String, JsThrow> {
    Ok("20030107".to_owned())
}

string_getter!(user_agent, user_agent);
string_getter!(vendor, vendor);

pub(crate) fn vendor_sub(_cx: &BindCx<'_>, _this: Rc<NavigatorData>) -> Result<String, JsThrow> {
    Ok(String::new())
}

pub(crate) fn language(_cx: &BindCx<'_>, this: Rc<NavigatorData>) -> Result<String, JsThrow> {
    Ok(this.languages.first().cloned().unwrap_or_default())
}

pub(crate) fn languages(cx: &BindCx<'_>, this: Rc<NavigatorData>) -> Result<JsValue, JsThrow> {
    if let Some(value) = cx.state.languages_js.borrow().clone() {
        return Ok(value);
    }
    let values: Vec<JsValue> = this
        .languages
        .iter()
        .cloned()
        .map(JsValue::String)
        .collect();
    let array = JsValue::Object(cx.scope.new_array(&values).map_err(JsThrow::from)?);
    let frozen = cx.freeze(&array)?;
    *cx.state.languages_js.borrow_mut() = Some(frozen.clone());
    Ok(frozen)
}

pub(crate) fn on_line(_cx: &BindCx<'_>, _this: Rc<NavigatorData>) -> Result<bool, JsThrow> {
    Ok(true)
}

pub(crate) fn cookie_enabled(_cx: &BindCx<'_>, _this: Rc<NavigatorData>) -> Result<bool, JsThrow> {
    Ok(true)
}

pub(crate) fn hardware_concurrency(
    _cx: &BindCx<'_>,
    this: Rc<NavigatorData>,
) -> Result<f64, JsThrow> {
    Ok(this.hardware_concurrency as f64)
}

pub(crate) fn webdriver(_cx: &BindCx<'_>, this: Rc<NavigatorData>) -> Result<bool, JsThrow> {
    Ok(this.webdriver)
}

pub(crate) fn max_touch_points(_cx: &BindCx<'_>, this: Rc<NavigatorData>) -> Result<f64, JsThrow> {
    Ok(f64::from(this.max_touch_points))
}

pub(crate) fn pdf_viewer_enabled(
    _cx: &BindCx<'_>,
    _this: Rc<NavigatorData>,
) -> Result<bool, JsThrow> {
    Ok(false)
}

pub(crate) fn plugins(cx: &BindCx<'_>, _this: Rc<NavigatorData>) -> Result<JsValue, JsThrow> {
    if let Some(value) = cx.state.plugins_js.borrow().clone() {
        return Ok(value);
    }
    let value = cx.new_plugin_array()?;
    *cx.state.plugins_js.borrow_mut() = Some(value.clone());
    Ok(value)
}

pub(crate) fn mime_types(cx: &BindCx<'_>, _this: Rc<NavigatorData>) -> Result<JsValue, JsThrow> {
    if let Some(value) = cx.state.mime_types_js.borrow().clone() {
        return Ok(value);
    }
    let value = cx.new_mime_type_array()?;
    *cx.state.mime_types_js.borrow_mut() = Some(value.clone());
    Ok(value)
}

pub(crate) fn java_enabled(_cx: &BindCx<'_>, _this: Rc<NavigatorData>) -> Result<bool, JsThrow> {
    Ok(false)
}
