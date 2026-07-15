//! `NodeList` implementation (live and static variants share storage).

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::imp::collection_items;

pub(crate) fn length(cx: &BindCx<'_>, this: u64) -> Result<f64, JsThrow> {
    Ok(collection_items(cx, this).len() as f64)
}

pub(crate) fn item(cx: &BindCx<'_>, this: u64, index: u32) -> Result<Option<NodeId>, JsThrow> {
    Ok(collection_items(cx, this).get(index as usize).copied())
}
