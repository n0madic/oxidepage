//! Base `Performance` timing surface.

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;

pub(crate) fn time_origin(cx: &BindCx<'_>, _this: u64) -> Result<f64, JsThrow> {
    Ok(cx.state.time_origin_epoch_ms)
}

pub(crate) fn now(cx: &BindCx<'_>, _this: u64) -> Result<f64, JsThrow> {
    Ok(cx.now_ms())
}

/// `performance.timing`: the realm-stable `PerformanceTiming` wrapper
/// (`[SameObject]`), minted on first access.
pub(crate) fn timing(cx: &BindCx<'_>, _this: u64) -> Result<JsValue, JsThrow> {
    if let Some(cached) = cx.state.performance_timing_js.borrow().clone() {
        return Ok(cached);
    }
    let wrapper = cx.new_performance_timing()?;
    *cx.state.performance_timing_js.borrow_mut() = Some(wrapper.clone());
    Ok(wrapper)
}
