//! `HTMLImageElement`: practical reflection plus the size/completion surface
//! backed by the layout engine's [`ImageStore`](oxidepage_layout::images::ImageStore).

use oxidepage_base::NodeId;
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::imp::geometry_support::flush_layout;
use crate::imp::reflect::{
    nullable_string_reflector, reflect_u32, reflect_url, set_u32, string_reflector, url_reflector,
};

url_reflector!(src, set_src, "src");
string_reflector!(srcset, set_srcset, "srcset");
string_reflector!(alt, set_alt, "alt");
string_reflector!(loading, set_loading, "loading");
string_reflector!(decoding, set_decoding, "decoding");
string_reflector!(referrer_policy, set_referrer_policy, "referrerpolicy");
nullable_string_reflector!(cross_origin, set_cross_origin, "crossorigin");

/// The absolute `src`, i.e. the key the image store is loaded under.
fn image_url(cx: &BindCx<'_>, this: NodeId) -> String {
    reflect_url(cx, this, "src")
}

/// `(intrinsic width, intrinsic height)` of the decoded image, if it decoded.
fn intrinsic(cx: &BindCx<'_>, this: NodeId) -> Option<(u32, u32)> {
    let url = image_url(cx, this);
    let layout = cx.state.layout.borrow();
    let image = layout.images().get(&url)?;
    Some((image.width, image.height))
}

pub(crate) fn natural_width(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(intrinsic(cx, this).map_or(0.0, |(w, _)| f64::from(w)))
}

pub(crate) fn natural_height(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(intrinsic(cx, this).map_or(0.0, |(_, h)| f64::from(h)))
}

/// True once the load has settled either way: decoded, or known-broken. An
/// `<img>` with no `src` is trivially complete.
pub(crate) fn complete(cx: &BindCx<'_>, this: NodeId) -> Result<bool, JsThrow> {
    let url = image_url(cx, this);
    if url.is_empty() {
        return Ok(true);
    }
    let layout = cx.state.layout.borrow();
    Ok(layout.images().get(&url).is_some() || layout.images().is_broken(&url))
}

/// The URL actually selected for display: the absolute `src` once it has
/// decoded, `""` while the load is still in flight (or failed). Selection from
/// `srcset` is not implemented.
pub(crate) fn current_src(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let url = image_url(cx, this);
    let decoded = cx.state.layout.borrow().images().get(&url).is_some();
    Ok(if decoded { url } else { String::new() })
}

/// The rendered used size, if the element has a box. Reads the padding box,
/// which for a replaced `<img>` with no padding is its content box.
///
/// This flushes styles and layout — the same cost `Element.clientWidth` pays —
/// so a plain property read here can trigger a reflow.
fn used_size(cx: &BindCx<'_>, this: NodeId) -> Option<(f32, f32)> {
    flush_layout(cx, this, |_, layout| {
        layout.client_box(this).map(|b| (b.width, b.height))
    })
}

/// `width`/`height` per HTML: the rendered used size when the image is being
/// rendered, otherwise the intrinsic size.
///
/// We add a third step the spec does not have — falling back to the content
/// attribute — so that a detached `<img width=32>` reports 32 instead of 0.
/// Detached elements have no box and, before the load starts, no intrinsic
/// size, and scripts that size images off `img.width` before insertion are
/// common enough to be worth the deviation.
fn dimension(
    cx: &BindCx<'_>,
    this: NodeId,
    used: impl FnOnce((f32, f32)) -> f32,
    natural: impl FnOnce((u32, u32)) -> u32,
    attr: &str,
) -> f64 {
    if let Some(size) = used_size(cx, this) {
        return f64::from(used(size)).round();
    }
    if let Some(size) = intrinsic(cx, this) {
        return f64::from(natural(size));
    }
    f64::from(reflect_u32(cx, this, attr))
}

pub(crate) fn width(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(dimension(cx, this, |(w, _)| w, |(w, _)| w, "width"))
}

pub(crate) fn set_width(cx: &BindCx<'_>, this: NodeId, value: u32) -> Result<(), JsThrow> {
    set_u32(cx, this, "width", value);
    Ok(())
}

pub(crate) fn height(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(dimension(cx, this, |(_, h)| h, |(_, h)| h, "height"))
}

pub(crate) fn set_height(cx: &BindCx<'_>, this: NodeId, value: u32) -> Result<(), JsThrow> {
    set_u32(cx, this, "height", value);
    Ok(())
}

/// `new Image(width, height)` — the `[LegacyFactoryFunction]`. Per HTML it is
/// `createElement("img")` on the *page* document plus the two optional content
/// attributes; the element is returned detached, and `src` is what starts a
/// load, so nothing is fetched here.
pub(crate) fn factory_image(
    cx: &BindCx<'_>,
    _call: &HostCall,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<JsValue, JsThrow> {
    let document = cx.state.dom.borrow().document();
    let img = crate::imp::document::create_element(cx, document, "img".to_owned(), JsValue::Null)?;
    if let Some(width) = width {
        set_u32(cx, img, "width", width);
    }
    if let Some(height) = height {
        set_u32(cx, img, "height", height);
    }
    cx.node_to_js(img)
}
