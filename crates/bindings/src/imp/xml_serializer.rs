//! `XMLSerializer`.
//!
//! A brand with no state: `serializeToString` is a pure function of the node it
//! is handed. The serializer itself lives in `dom` beside the HTML one — see
//! [`oxidepage_dom::serialize::xml_serialize`] for the namespace prefix
//! generation it deliberately does not do.

use oxidepage_base::NodeId;
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::HostData;

pub(crate) fn constructor(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.new_slab_object("XMLSerializer", HostData::XmlSerializer)
}

pub(crate) fn serialize_to_string(
    cx: &BindCx<'_>,
    _this: u64,
    root: NodeId,
) -> Result<String, JsThrow> {
    Ok(oxidepage_dom::serialize::xml_serialize(
        &cx.state.dom.borrow(),
        root,
    ))
}
