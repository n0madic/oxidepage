//! `HTMLAreaElement`. As with `<a>`, `href` and the URL decomposition come from
//! [`crate::imp::html_hyperlink_element_utils`].

use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;
use crate::imp::reflect::string_reflector;

string_reflector!(alt, set_alt, "alt");
string_reflector!(coords, set_coords, "coords");
string_reflector!(shape, set_shape, "shape");
string_reflector!(target, set_target, "target");
string_reflector!(download, set_download, "download");
string_reflector!(rel, set_rel, "rel");
string_reflector!(referrer_policy, set_referrer_policy, "referrerpolicy");

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
