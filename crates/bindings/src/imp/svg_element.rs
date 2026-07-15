//! `SVGElement` implementation. Its only member is `dataset` — the
//! `HTMLOrSVGElement` mixin's `DOMStringMap`, backed exactly like
//! [`crate::imp::html_element::dataset`].

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;

/// `svgElement.dataset` — the live `DOMStringMap` over the element's `data-*`
/// attributes. `[SameObject]`, so repeated reads return one cached Proxy.
pub(crate) fn dataset(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "dataset", |cx| cx.new_dataset(this))
}
