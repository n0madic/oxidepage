//! `StyleSheetList` (`document.styleSheets`) implementation.

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::cssdata::SheetData;
use crate::cx::BindCx;

/// The owner nodes of `document`'s author sheets, in document order.
///
/// Only a *rendered* document has a stylist. An inert document's `<style>` and
/// `<link>` elements are never connected, so none of its sheets are registered
/// anywhere — reporting a stylist's list for it would be a lie, so it gets an
/// honestly empty one.
///
/// The one compared against is **this realm's** document, not the page's:
/// `cx.state.style` already *is* this browsing context's engine (ADR-0035 D1),
/// so the page-wide comparison made `document.styleSheets` permanently empty
/// inside every frame while its own sheets sat registered right there. A parent
/// realm reading `iframe.contentDocument.styleSheets` still gets an empty list,
/// because the engine it would have to consult is not this realm's — the same
/// documented limit `document.location` has.
fn sheet_owners(cx: &BindCx<'_>, document: NodeId) -> Vec<NodeId> {
    if document != cx.state.frame.document() {
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
