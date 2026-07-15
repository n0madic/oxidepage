//! `HTMLTemplateElement`.

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;

/// `template.content`: the template's contents, a `DocumentFragment` that is
/// **not** part of the document tree.
///
/// The parser already puts a `<template>`'s children there rather than in its
/// child list (which is why `template.childNodes` is empty while `innerHTML`
/// still serialises them), so this getter only has to hand the fragment to
/// script. `ensure_template_contents` creates it on demand for a template that
/// came from `document.createElement` rather than the parser.
///
/// `[SameObject]` holds without any caching here: the wrapper cache is keyed by
/// arena index, so the same fragment node always yields the same JS object.
pub(crate) fn content(cx: &BindCx<'_>, this: NodeId) -> Result<NodeId, JsThrow> {
    Ok(cx.state.dom.borrow_mut().ensure_template_contents(this))
}
