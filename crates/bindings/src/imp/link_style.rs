//! The `LinkStyle` mixin: `<style>.sheet` and `<link>.sheet`.

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;

/// The element's associated CSS style sheet, or `null` while it has none.
///
/// It is the same object `document.styleSheets` lists, because both come from
/// the one `[SameObject]` slot per owner node. A sheet exists only while the
/// element is in this realm's rendered document (or one of its shadow trees)
/// and, for a `<link>`, once it is a loaded style sheet. An element of
/// another frame's document answers `null` here, the documented limit
/// `iframe.contentDocument.styleSheets` shares: that sheet is registered with
/// the other frame's engine, not this realm's.
pub(crate) fn sheet(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    // A `<style>` inserted or removed earlier in this script must show (or
    // lose) its sheet now, not at the next style flush.
    super::css_style_declaration::flush_inline_styles(cx);
    if cx.sheet_for(this).is_none() {
        return Ok(JsValue::Null);
    }
    cx.node_sheet_wrapper(this)
}
