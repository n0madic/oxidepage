//! `DocumentType` implementation.

use oxidepage_base::NodeId;
use oxidepage_dom::NodeData;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;

fn parts(cx: &BindCx<'_>, this: NodeId) -> Result<(String, String, String), JsThrow> {
    let dom = cx.state.dom.borrow();
    match dom.node(this).data() {
        NodeData::Doctype {
            name,
            public_id,
            system_id,
        } => Ok((
            name.to_string(),
            public_id.to_string(),
            system_id.to_string(),
        )),
        _ => Err(JsThrow::Type("receiver is not a DocumentType".into())),
    }
}

pub(crate) fn name(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(parts(cx, this)?.0)
}

pub(crate) fn public_id(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(parts(cx, this)?.1)
}

pub(crate) fn system_id(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(parts(cx, this)?.2)
}
