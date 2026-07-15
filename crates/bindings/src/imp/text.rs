//! `Text` implementation.

use oxidepage_base::{DomExceptionKind, NodeId};
use oxidepage_dom::NodeKind;
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::imp::character_data::data_of;

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    data: String,
) -> Result<JsValue, JsThrow> {
    let node = cx.state.dom.borrow_mut().create_text(data.into());
    cx.node_to_js(node)
}

pub(crate) fn split_text(cx: &BindCx<'_>, this: NodeId, offset: u32) -> Result<NodeId, JsThrow> {
    let current: Vec<u16> = data_of(cx, this).encode_utf16().collect();
    let offset = offset as usize;
    if offset > current.len() {
        return Err(cx.dom_throw(
            DomExceptionKind::IndexSizeError,
            "offset is past the end of the data",
        ));
    }
    let head = String::from_utf16_lossy(&current[..offset]);
    let tail = String::from_utf16_lossy(&current[offset..]);
    let mut dom = cx.state.dom.borrow_mut();
    // The new node is created in `this`'s node document (not the page's — the
    // node may be a detached child of a second document, where an insertion
    // would never come along to adopt it), and splitting a CDATASection yields
    // a CDATASection.
    let owner = dom.node_document(this);
    let new_node = if dom.node(this).data().kind() == NodeKind::CdataSection {
        dom.create_cdata_section_in(owner, tail.into())
    } else {
        dom.create_text_in(owner, tail.into())
    };
    let parent = dom.node(this).parent();
    if let Some(parent) = parent {
        let next = dom.node(this).next_sibling();
        dom.insert_before(parent, new_node, next)
            .map_err(|e| cx.dom_exception(e))?;
    }
    dom.set_character_data(this, head.into());
    Ok(new_node)
}

pub(crate) fn whole_text(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let dom = cx.state.dom.borrow();
    // Walk to the first contiguous text sibling, then concatenate the run.
    // "Contiguous Text nodes" includes CDATASections — they are Text nodes.
    let mut first = this;
    while let Some(prev) = dom.node(first).prev_sibling() {
        if !dom.node(prev).is_text() {
            break;
        }
        first = prev;
    }
    let mut out = String::new();
    let mut current = Some(first);
    while let Some(id) = current {
        if !dom.node(id).is_text() {
            break;
        }
        if let Some(data) = dom.node(id).character_data() {
            out.push_str(data);
        }
        current = dom.node(id).next_sibling();
    }
    Ok(out)
}
