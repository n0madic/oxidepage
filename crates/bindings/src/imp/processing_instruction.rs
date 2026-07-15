//! `ProcessingInstruction` implementation.

use oxidepage_base::NodeId;
use oxidepage_dom::NodeData;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;

pub(crate) fn target(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let dom = cx.state.dom.borrow();
    match dom.node(this).data() {
        NodeData::ProcessingInstruction { target, .. } => Ok(target.to_string()),
        _ => Err(JsThrow::Type(
            "receiver is not a ProcessingInstruction".into(),
        )),
    }
}
