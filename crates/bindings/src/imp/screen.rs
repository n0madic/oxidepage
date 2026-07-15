//! `Screen` immutable virtual-display profile.

use std::rc::Rc;

use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::state::ScreenData;

pub(crate) fn width(_cx: &BindCx<'_>, this: Rc<ScreenData>) -> Result<f64, JsThrow> {
    Ok(f64::from(this.width))
}

pub(crate) fn height(_cx: &BindCx<'_>, this: Rc<ScreenData>) -> Result<f64, JsThrow> {
    Ok(f64::from(this.height))
}

pub(crate) fn avail_width(_cx: &BindCx<'_>, this: Rc<ScreenData>) -> Result<f64, JsThrow> {
    Ok(f64::from(this.avail_width))
}

pub(crate) fn avail_height(_cx: &BindCx<'_>, this: Rc<ScreenData>) -> Result<f64, JsThrow> {
    Ok(f64::from(this.avail_height))
}

pub(crate) fn color_depth(_cx: &BindCx<'_>, this: Rc<ScreenData>) -> Result<f64, JsThrow> {
    Ok(f64::from(this.color_depth))
}

pub(crate) fn pixel_depth(_cx: &BindCx<'_>, this: Rc<ScreenData>) -> Result<f64, JsThrow> {
    Ok(f64::from(this.pixel_depth))
}
