//! `Window` methods backed by the realm global object.

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::EventTargetKey;

pub(crate) fn match_media(
    cx: &BindCx<'_>,
    _this: EventTargetKey,
    query: String,
) -> Result<JsValue, JsThrow> {
    let matches = cx.state.style.borrow().media_query_matches(&query);
    cx.new_media_query_list(query, matches)
}
