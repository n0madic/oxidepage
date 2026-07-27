//! `PopStateEvent`: fired at the window when a session-history traversal stays
//! within the current document.
//!
//! `state` reuses the single extra-value slot on [`EventData::detail`] — see
//! its doc comment for the three readers that share it.

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::EventData;
use crate::imp::event::{EventRef, parse_event_init};

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    event_type: String,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let (bubbles, cancelable, composed) = parse_event_init(cx, &init);
    let mut data = EventData::new(event_type, bubbles, cancelable, composed);
    data.time_stamp = cx.now_ms();
    if let JsValue::Object(obj) = &init
        && let Ok(state) = cx.scope.get(obj, "state")
        && !state.is_undefined()
    {
        data.detail = state;
    }
    let (value, _) = cx.new_event_object("PopStateEvent", data)?;
    Ok(value)
}

pub(crate) fn state(_cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    Ok(this.borrow().detail.clone())
}
