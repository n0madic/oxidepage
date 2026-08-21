//! `HTMLCanvasElement`: `getContext` reports unsupported contexts as `null`.
//!
//! The engine implements no canvas rendering context — neither `2d` nor any
//! WebGL flavour. The HTML spec still requires the *method* to exist and to
//! return `null` when the requested contextId is not supported; that null is
//! exactly what pages' feature-detection idioms
//! (`canvas.getContext('webgl') || …`) branch on, so a page degrades
//! gracefully instead of dying on a `TypeError` from an absent member.

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;

pub(crate) fn get_context(
    _cx: &BindCx<'_>,
    _this: NodeId,
    _context_id: String,
    _options: JsValue,
) -> Result<JsValue, JsThrow> {
    Ok(JsValue::Null)
}
