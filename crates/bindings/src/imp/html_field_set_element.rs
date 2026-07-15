//! `HTMLFieldSetElement`. `elements` is the fieldset's *own* listed descendants
//! (not the form's), so it is a plain descendant filter rather than a form
//! ownership query.

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;
use crate::imp::reflect::{bool_reflector, string_reflector};

pub(crate) use crate::imp::form_support::form;

string_reflector!(name, set_name, "name");
bool_reflector!(disabled, set_disabled, "disabled");

pub(crate) fn r#type(_cx: &BindCx<'_>, _this: NodeId) -> Result<String, JsThrow> {
    Ok("fieldset".to_owned())
}

pub(crate) fn elements(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "elements", |cx| {
        cx.new_collection("HTMLCollection", CollectionData::FieldSetControls(this))
    })
}
