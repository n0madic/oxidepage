//! `AbortController`: constructor, `signal` accessor, and `abort()`.

use std::rc::Rc;

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::imp::abort_signal;
use crate::state::AbortSignalData;

pub(crate) fn constructor(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.new_abort_controller()
}

pub(crate) fn signal(_cx: &BindCx<'_>, this: Rc<AbortSignalData>) -> Result<JsValue, JsThrow> {
    this.wrapper
        .borrow()
        .clone()
        .ok_or_else(|| JsThrow::Type("AbortController signal is not installed".into()))
}

pub(crate) fn abort(
    cx: &BindCx<'_>,
    this: Rc<AbortSignalData>,
    reason: JsValue,
) -> Result<(), JsThrow> {
    abort_signal::signal_abort(cx, &this, reason)
}
