//! `DocumentFragment` implementation.

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;

pub(crate) fn constructor(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    let node = cx.state.dom.borrow_mut().create_document_fragment();
    cx.node_to_js(node)
}
