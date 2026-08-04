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
        let node = cx.this_element(&submitter).map_err(|_| {
            JsThrow::Type("SubmitEvent constructor: submitter is not an HTMLElement".into())
        })?;
        data.detail =
            crate::events::EventDetail::Node(crate::events::PinnedNode::new(&cx.state.dom, node));
    }
    let (value, _) = cx.new_event_object("SubmitEvent", data)?;
    Ok(value)
}

/// The submitter node, pinned by the event and resolved through **this**
/// world's wrapper cache.
pub(crate) fn submitter(cx: &BindCx<'_>, this: EventRef) -> Result<Option<NodeId>, JsThrow> {
    let id = match &this.borrow().detail {
        crate::events::EventDetail::Node(node) => Some(node.id()),
        _ => None,
    };
    // The pin keeps the node alive, so this can only miss after a navigation
    // replaced the arena — where `None` is the honest answer.
    Ok(id.filter(|id| cx.state.dom.borrow().get(*id).is_some()))
}

/// Builds a trusted `submit` event for [`crate::imp::form_submit`].
///
/// No wrapper is minted here: the dispatch mints one per world that turns out
/// to have a listener on the path (ADR-0033 D6).
pub(crate) fn new_trusted_data(
    cx: &BindCx<'_>,
    submitter: Option<NodeId>,
) -> Result<EventRef, JsThrow> {
    let mut data = EventData::new(
        "submit".to_owned(),
        /* bubbles */ true,
        /* cancelable */ true,
        /* composed */ false,
    );
    data.is_trusted = true;
    data.time_stamp = cx.now_ms();
    if let Some(node) = submitter {
        data.detail =
            crate::events::EventDetail::Node(crate::events::PinnedNode::new(&cx.state.dom, node));
    }
    Ok(cx.new_event_data("SubmitEvent", data))
}
