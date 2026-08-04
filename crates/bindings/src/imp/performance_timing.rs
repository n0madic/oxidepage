//! `PerformanceTiming` (Navigation Timing Level 1) attribute getters.
//!
//! Each getter reads the milestone recorded on [`WorldState::timing`] and
//! rounds it to whole milliseconds (the IDL type is `unsigned long long`).
//! `unload*`/`redirect*`/`secureConnectionStart` are always `0` (v1: no
//! distinct network phases for synchronously injected HTML).
//!
//! [`WorldState::timing`]: crate::state::WorldState::timing

use oxidepage_js::JsThrow;

use crate::cx::BindCx;

/// Rounds an epoch-ms `f64` milestone to a whole-millisecond value.
fn ms(value: f64) -> f64 {
    value.round()
}

macro_rules! timing_getter {
    ($name:ident, $field:ident) => {
        pub(crate) fn $name(cx: &BindCx<'_>, _this: u64) -> Result<f64, JsThrow> {
            Ok(ms(cx.state.page.timing.borrow().$field))
        }
    };
}

macro_rules! zero_getter {
    ($name:ident) => {
        pub(crate) fn $name(_cx: &BindCx<'_>, _this: u64) -> Result<f64, JsThrow> {
            Ok(0.0)
        }
    };
}

timing_getter!(navigation_start, navigation_start);
zero_getter!(unload_event_start);
zero_getter!(unload_event_end);
zero_getter!(redirect_start);
zero_getter!(redirect_end);
timing_getter!(fetch_start, fetch_start);
timing_getter!(domain_lookup_start, domain_lookup_start);
timing_getter!(domain_lookup_end, domain_lookup_end);
timing_getter!(connect_start, connect_start);
timing_getter!(connect_end, connect_end);
zero_getter!(secure_connection_start);
timing_getter!(request_start, request_start);
timing_getter!(response_start, response_start);
timing_getter!(response_end, response_end);
timing_getter!(dom_loading, dom_loading);
timing_getter!(dom_interactive, dom_interactive);
timing_getter!(
    dom_content_loaded_event_start,
    dom_content_loaded_event_start
);
timing_getter!(dom_content_loaded_event_end, dom_content_loaded_event_end);
timing_getter!(dom_complete, dom_complete);
timing_getter!(load_event_start, load_event_start);
timing_getter!(load_event_end, load_event_end);
