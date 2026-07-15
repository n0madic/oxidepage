//! `HTMLAnchorElement`. The `href` / URL-decomposition half lives in
//! [`crate::imp::html_hyperlink_element_utils`] (IDL mixin `includes`).

use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;
use crate::imp::reflect::string_reflector;

string_reflector!(target, set_target, "target");
string_reflector!(download, set_download, "download");
string_reflector!(rel, set_rel, "rel");
string_reflector!(hreflang, set_hreflang, "hreflang");
string_reflector!(r#type, set_type, "type");
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

pub(crate) fn text(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(cx.state.dom.borrow().text_content(this))
}

pub(crate) fn set_text(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    super::node::set_text_content(cx, this, Some(value))
}
