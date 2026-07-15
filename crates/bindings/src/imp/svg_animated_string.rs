//! `SVGAnimatedString`: an element attribute seen as a base/animated value pair.
//!
//! Live over the attribute (like `DOMTokenList`): every read goes to the DOM, so
//! nothing has to be invalidated when the attribute changes.

use html5ever::ns;
use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_dom::node::attr_name;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;

/// The reflected value: the SVG2 attribute (`href`), falling back to the SVG 1.1
/// form (`xlink:href`), which is still what a great deal of SVG in the wild
/// carries. Matched by namespace rather than by prefix — the prefix is not
/// fixed, the namespace is.
fn reflected(cx: &BindCx<'_>, element: NodeId, attr: &LocalName) -> String {
    let dom = cx.state.dom.borrow();
    let Some(el) = dom.get(element).and_then(|node| node.as_element()) else {
        return String::new();
    };
    if let Some(value) = el.attr(&attr_name(attr.clone())) {
        return value.to_string();
    }
    el.attrs()
        .iter()
        .find(|a| a.name.ns == ns!(xlink) && a.name.local == *attr)
        .map(|a| a.value.to_string())
        .unwrap_or_default()
}

pub(crate) fn base_val(cx: &BindCx<'_>, this: (NodeId, LocalName)) -> Result<String, JsThrow> {
    let (element, attr) = this;
    Ok(reflected(cx, element, &attr))
}

pub(crate) fn set_base_val(
    cx: &BindCx<'_>,
    this: (NodeId, LocalName),
    value: String,
) -> Result<(), JsThrow> {
    let (element, attr) = this;
    // The write always lands on the SVG2 attribute, as in browsers: `xlink:href`
    // is only ever read as a fallback, never created.
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(element, attr_name(attr), value.into());
    Ok(())
}

/// Equal to `baseVal` whenever no animation is in effect (spec), and SMIL
/// animation is not implemented, so it always is.
pub(crate) fn anim_val(cx: &BindCx<'_>, this: (NodeId, LocalName)) -> Result<String, JsThrow> {
    let (element, attr) = this;
    Ok(reflected(cx, element, &attr))
}
