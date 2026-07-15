//! `IntersectionObserverEntry` attribute getters (values precomputed at delivery).

use std::rc::Rc;

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::IoEntryView;

pub(crate) fn time(_cx: &BindCx<'_>, this: Rc<IoEntryView>) -> Result<f64, JsThrow> {
    Ok(this.time)
}

pub(crate) fn root_bounds(_cx: &BindCx<'_>, this: Rc<IoEntryView>) -> Result<JsValue, JsThrow> {
    Ok(this.root_bounds.clone())
}

pub(crate) fn bounding_client_rect(
    _cx: &BindCx<'_>,
    this: Rc<IoEntryView>,
) -> Result<JsValue, JsThrow> {
    Ok(this.bounding_client_rect.clone())
}

pub(crate) fn intersection_rect(
    _cx: &BindCx<'_>,
    this: Rc<IoEntryView>,
) -> Result<JsValue, JsThrow> {
    Ok(this.intersection_rect.clone())
}

pub(crate) fn is_intersecting(_cx: &BindCx<'_>, this: Rc<IoEntryView>) -> Result<bool, JsThrow> {
    Ok(this.is_intersecting)
}

pub(crate) fn intersection_ratio(_cx: &BindCx<'_>, this: Rc<IoEntryView>) -> Result<f64, JsThrow> {
    Ok(this.intersection_ratio)
}

pub(crate) fn target(_cx: &BindCx<'_>, this: Rc<IoEntryView>) -> Result<JsValue, JsThrow> {
    Ok(this.target.clone())
}
