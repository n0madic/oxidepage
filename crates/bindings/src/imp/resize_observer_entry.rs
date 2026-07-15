//! `ResizeObserverEntry` attribute getters (all values precomputed at delivery).

use std::rc::Rc;

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::RoEntryView;

pub(crate) fn target(_cx: &BindCx<'_>, this: Rc<RoEntryView>) -> Result<JsValue, JsThrow> {
    Ok(this.target.clone())
}

pub(crate) fn content_rect(_cx: &BindCx<'_>, this: Rc<RoEntryView>) -> Result<JsValue, JsThrow> {
    Ok(this.content_rect.clone())
}

pub(crate) fn border_box_size(_cx: &BindCx<'_>, this: Rc<RoEntryView>) -> Result<JsValue, JsThrow> {
    Ok(this.border_box_size.clone())
}

pub(crate) fn content_box_size(
    _cx: &BindCx<'_>,
    this: Rc<RoEntryView>,
) -> Result<JsValue, JsThrow> {
    Ok(this.content_box_size.clone())
}

pub(crate) fn device_pixel_content_box_size(
    _cx: &BindCx<'_>,
    this: Rc<RoEntryView>,
) -> Result<JsValue, JsThrow> {
    Ok(this.device_pixel_content_box_size.clone())
}
