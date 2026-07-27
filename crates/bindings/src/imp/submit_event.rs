//! `SubmitEvent`: fired at a form when submission starts, carrying the element
//! that triggered it.
//!
//! `submitter` reuses the single extra-value slot on [`EventData::detail`] —
//! see its doc comment for the three readers that share it. It is held as the
//! submitter's *wrapper*, not its id: a wrapper pins its node, so an event
//! parked in a listener's closure cannot be left naming a freed slot, and the
//! id is recovered (generation-checked) on read.

use oxidepage_base::NodeId;
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
        && let Ok(submitter) = cx.scope.get(obj, "submitter")
        && !submitter.is_nullish()
    {
        // The dictionary member is typed `HTMLElement?`; anything else is a
        // TypeError, as the WebIDL conversion would raise.
        cx.this_element(&submitter).map_err(|_| {
            JsThrow::Type("SubmitEvent constructor: submitter is not an HTMLElement".into())
        })?;
        data.detail = submitter;
    }
    let (value, _) = cx.new_event_object("SubmitEvent", data)?;
    Ok(value)
}

pub(crate) fn submitter(cx: &BindCx<'_>, this: EventRef) -> Result<Option<NodeId>, JsThrow> {
    let value = this.borrow().detail.clone();
    if value.is_nullish() {
        return Ok(None);
    }
    Ok(cx.this_element(&value).ok())
}

/// Builds a trusted `submit` event for [`crate::imp::form_submit`].
pub(crate) fn new_trusted(
    cx: &BindCx<'_>,
    submitter: Option<NodeId>,
) -> Result<(JsValue, EventRef), JsThrow> {
    let mut data = EventData::new(
        "submit".to_owned(),
        /* bubbles */ true,
        /* cancelable */ true,
        /* composed */ false,
    );
    data.is_trusted = true;
    data.time_stamp = cx.now_ms();
    if let Some(node) = submitter {
        data.detail = cx.node_to_js(node)?;
    }
    cx.new_event_object("SubmitEvent", data)
}
