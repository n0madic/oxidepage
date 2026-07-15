//! `HTMLButtonElement`. A button's `value` is a plain reflection — there is no
//! dirty value flag on a button.

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::imp::reflect::{bool_reflector, reflect_string, set_string, string_reflector};

pub(crate) use crate::imp::form_support::{form, labels};

string_reflector!(name, set_name, "name");
string_reflector!(value, set_value, "value");
bool_reflector!(disabled, set_disabled, "disabled");

/// `button.type` is limited to `submit`/`reset`/`button`, defaulting to
/// `submit` — the "missing value default".
pub(crate) fn r#type(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let raw = reflect_string(cx, this, "type").to_ascii_lowercase();
    Ok(match raw.as_str() {
        "reset" | "button" => raw,
        _ => "submit".to_owned(),
    })
}

pub(crate) fn set_type(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    set_string(cx, this, "type", value);
    Ok(())
}
