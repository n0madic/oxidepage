//! Empty `MimeTypeArray`: no legacy plugins means no plugin MIME types.

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;

pub(crate) fn item(_cx: &BindCx<'_>, _this: u64, _index: u32) -> Result<JsValue, JsThrow> {
    Ok(JsValue::Null)
}

pub(crate) fn named_item(_cx: &BindCx<'_>, _this: u64, _name: String) -> Result<JsValue, JsThrow> {
    Ok(JsValue::Null)
}

pub(crate) fn length(_cx: &BindCx<'_>, _this: u64) -> Result<f64, JsThrow> {
    Ok(0.0)
}
