//! `MutationRecord` implementation.

use std::rc::Rc;

use oxidepage_base::NodeId;
use oxidepage_dom::MutationRecordType;
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;
use crate::state::RecordView;

pub(crate) fn r#type(_cx: &BindCx<'_>, this: Rc<RecordView>) -> Result<String, JsThrow> {
    Ok(match this.record.record_type {
        MutationRecordType::ChildList => "childList".to_owned(),
        MutationRecordType::Attributes => "attributes".to_owned(),
        MutationRecordType::CharacterData => "characterData".to_owned(),
    })
}

pub(crate) fn target(_cx: &BindCx<'_>, this: Rc<RecordView>) -> Result<NodeId, JsThrow> {
    Ok(this.record.target)
}

fn node_list(
    cx: &BindCx<'_>,
    cache: &std::cell::RefCell<Option<JsValue>>,
    nodes: &[NodeId],
) -> Result<JsValue, JsThrow> {
    if let Some(cached) = cache.borrow().clone() {
        return Ok(cached);
    }
    let list = cx.new_collection("NodeList", CollectionData::StaticNodes(nodes.to_vec()))?;
    *cache.borrow_mut() = Some(list.clone());
    Ok(list)
}

pub(crate) fn added_nodes(cx: &BindCx<'_>, this: Rc<RecordView>) -> Result<JsValue, JsThrow> {
    node_list(cx, &this.added_nodes_js, &this.record.added_nodes)
}

pub(crate) fn removed_nodes(cx: &BindCx<'_>, this: Rc<RecordView>) -> Result<JsValue, JsThrow> {
    node_list(cx, &this.removed_nodes_js, &this.record.removed_nodes)
}

fn live_or_none(cx: &BindCx<'_>, id: Option<NodeId>) -> Option<NodeId> {
    id.filter(|&id| cx.state.dom.borrow().get(id).is_some())
}

pub(crate) fn previous_sibling(
    cx: &BindCx<'_>,
    this: Rc<RecordView>,
) -> Result<Option<NodeId>, JsThrow> {
    Ok(live_or_none(cx, this.record.previous_sibling))
}

pub(crate) fn next_sibling(
    cx: &BindCx<'_>,
    this: Rc<RecordView>,
) -> Result<Option<NodeId>, JsThrow> {
    Ok(live_or_none(cx, this.record.next_sibling))
}

pub(crate) fn attribute_name(
    _cx: &BindCx<'_>,
    this: Rc<RecordView>,
) -> Result<Option<String>, JsThrow> {
    Ok(this.record.attribute_name.as_ref().map(|n| n.to_string()))
}

pub(crate) fn attribute_namespace(
    _cx: &BindCx<'_>,
    this: Rc<RecordView>,
) -> Result<Option<String>, JsThrow> {
    Ok(this
        .record
        .attribute_namespace
        .as_ref()
        .map(|n| n.to_string()))
}

pub(crate) fn old_value(_cx: &BindCx<'_>, this: Rc<RecordView>) -> Result<Option<String>, JsThrow> {
    Ok(this.record.old_value.as_ref().map(|v| v.to_string()))
}
