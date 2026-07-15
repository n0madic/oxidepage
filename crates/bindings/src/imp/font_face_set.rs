//! `FontFaceSet` implementation (CSS Font Loading Module Level 3, trimmed):
//! `document.fonts.ready`/`.status` only. See `css_font_loading.webidl` for
//! why nothing else is installed.

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::ReadyState;

pub(crate) fn status(cx: &BindCx<'_>, _this: u64) -> Result<String, JsThrow> {
    Ok(if cx.state.fonts_loading.get() {
        "loading"
    } else {
        "loaded"
    }
    .to_owned())
}

/// `ready`: a promise that resolves with the `FontFaceSet` once every
/// `@font-face` load this document started has settled.
///
/// Re-evaluated on every read rather than cached as the single WebIDL
/// `Promise`-attribute value: a read *after* fonts have already settled must
/// still return a resolved promise, not a stale pending one left over from an
/// earlier read taken while a load was in flight.
///
/// "Settled" requires the document to have finished parsing, not just
/// `fonts_loading == false` — a `<style>` with `@font-face` later in the
/// document has not been scanned yet while the parser is still running, so
/// resolving on `fonts_loading` alone could fire before that load even
/// starts. `Page::settle_font_ready` (the resolution side, in the page
/// crate) applies the same two-part condition.
pub(crate) fn ready(cx: &BindCx<'_>, _this: u64) -> Result<JsValue, JsThrow> {
    let value = cx
        .state
        .font_face_set_js
        .borrow()
        .clone()
        .expect("ready() is reachable only through the cached document.fonts wrapper");
    let settled = !cx.state.fonts_loading.get() && cx.state.ready_state() != ReadyState::Loading;
    if settled {
        return cx.resolved_promise(value);
    }
    let (promise, resolve, _reject) = cx.make_promise()?;
    cx.state.font_ready_resolvers.borrow_mut().push(resolve);
    Ok(promise)
}

/// `load(font, text)`: a promise that resolves with the loaded `FontFace`
/// sequence once every `@font-face` load this document started has settled.
///
/// The engine has no per-request font loader: a `@font-face`'s matching family
/// is fetched through the normal font pipeline (the same one `ready` awaits), so
/// `load` resolves on that same settle condition and yields an *empty* sequence
/// — callers (angular.dev's Material Symbols gate among them) await completion,
/// not the returned faces. `font`/`text` are accepted for API shape but not used
/// to scope the wait, which conservatively resolves no earlier than needed.
pub(crate) fn load(
    cx: &BindCx<'_>,
    _this: u64,
    _font: String,
    _text: String,
) -> Result<JsValue, JsThrow> {
    let empty = cx
        .scope
        .eval("[]", "oxidepage:font-load")
        .map_err(JsThrow::from)?;
    let settled = !cx.state.fonts_loading.get() && cx.state.ready_state() != ReadyState::Loading;
    if settled {
        return cx.resolved_promise(empty);
    }
    let (promise, resolve, _reject) = cx.make_promise()?;
    cx.state.font_load_resolvers.borrow_mut().push(resolve);
    Ok(promise)
}
