//! `SVGAElement`: the SVG `<a>`. Its `href` is an `SVGAnimatedString`, not a
//! string — script branches on exactly that difference from an HTML `<a>`.

use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::HostData;

pub(crate) fn href(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "href", |cx| {
        cx.new_slab_object(
            "SVGAnimatedString",
            HostData::SvgAnimatedString {
                element: this,
                attr: LocalName::from("href"),
            },
        )
    })
}
