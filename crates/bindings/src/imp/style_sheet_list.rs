//! `StyleSheetList` (`document.styleSheets`) implementation.

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::cssdata::SheetData;
use crate::cx::BindCx;

/// The owner nodes of `document`'s author sheets, in document order.
///
/// Only the page document has a stylist. A second document's `<style>` and
/// `<link>` elements are never connected, so none of its sheets are registered
/// anywhere — reporting the page's list for it would be a lie, so it gets an
/// honestly empty one.
fn sheet_owners(cx: &BindCx<'_>, document: NodeId) -> Vec<NodeId> {
    if document != cx.state.dom.borrow().document() {
        return Vec::new();
    }
    cx.state
        .style
        .borrow()
        .author_sheets()
        .map(|(owner, _)| owner)
        .collect()
}

pub(crate) fn length(cx: &BindCx<'_>, document: NodeId) -> Result<f64, JsThrow> {
    Ok(sheet_owners(cx, document).len() as f64)
}

pub(crate) fn item(cx: &BindCx<'_>, document: NodeId, index: u32) -> Result<JsValue, JsThrow> {
    match sheet_owners(cx, document).get(index as usize) {
        Some(&owner) => cx.same_object(owner, "cssom-sheet", move |cx| {
            cx.new_style_sheet(SheetData::Node { owner })
        }),
        None => Ok(JsValue::Null),
    }
}
