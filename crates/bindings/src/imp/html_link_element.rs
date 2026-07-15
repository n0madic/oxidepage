//! `HTMLLinkElement`: practical reflection only. Loading and sheet ownership
//! stay with the page/style engine.

use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;
use crate::imp::reflect::{
    bool_reflector, nullable_string_reflector, string_reflector, url_reflector,
};

url_reflector!(href, set_href, "href");
string_reflector!(rel, set_rel, "rel");
string_reflector!(media, set_media, "media");
string_reflector!(r#type, set_type, "type");
string_reflector!(hreflang, set_hreflang, "hreflang");
// `as` is a Rust keyword; the content attribute is spelled the same.
string_reflector!(r#as, set_as, "as");
nullable_string_reflector!(cross_origin, set_cross_origin, "crossorigin");
// Reflecting the content attribute's presence. The spec's `disabled` also
// gates whether the sheet applies; that path is owned by the style engine,
// which reads the attribute directly.
bool_reflector!(disabled, set_disabled, "disabled");

pub(crate) fn rel_list(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "relList", |cx| {
        cx.new_collection(
            "DOMTokenList",
            CollectionData::TokenList {
                element: this,
                attr: LocalName::from("rel"),
            },
        )
    })
}
