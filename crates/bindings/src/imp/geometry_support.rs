//! Shared layout-flush plumbing for the CSSOM-View geometry bindings.
//!
//! Every geometry read mirrors the `getComputedStyle` path: flush pending
//! `<style>`/`<link>` updates, then bring the layout up to date
//! (`LayoutEngine::reflow` guards with a version stamp, so clean reads are
//! cheap).
//!
//! Invariant: the closures run while `dom`, `style`, and `layout` are all
//! borrowed from `WorldState` — reflow and the geometry queries must never
//! call back into JS bindings.

use oxidepage_base::NodeId;
use oxidepage_dom::DomTree;
use oxidepage_layout::LayoutEngine;

use crate::cx::BindCx;
use crate::imp::css_style_declaration::flush_inline_styles;
use crate::state::RectData;

/// Flushes styles + layout, then runs `f` with read access to the DOM and
/// the laid-out engine.
pub(crate) fn flush_layout<R>(cx: &BindCx<'_>, f: impl FnOnce(&DomTree, &LayoutEngine) -> R) -> R {
    flush_inline_styles(cx);
    let mut dom = cx.state.dom.borrow_mut();
    let mut style = cx.state.style.borrow_mut();
    let mut layout = cx.state.layout.borrow_mut();
    layout.reflow(&mut dom, &mut style);
    f(&dom, &layout)
}

/// Like [`flush_layout`], but with write access to the layout engine (scroll
/// offset writes).
pub(crate) fn flush_layout_mut<R>(
    cx: &BindCx<'_>,
    f: impl FnOnce(&DomTree, &mut LayoutEngine) -> R,
) -> R {
    flush_inline_styles(cx);
    let mut dom = cx.state.dom.borrow_mut();
    let mut style = cx.state.style.borrow_mut();
    let mut layout = cx.state.layout.borrow_mut();
    layout.reflow(&mut dom, &mut style);
    f(&dom, &mut layout)
}

/// Converts a layout rect (f32 CSS px) to `DOMRect` backing data.
pub(crate) fn rect_data(rect: oxidepage_base::Rect) -> RectData {
    RectData {
        x: f64::from(rect.origin.x),
        y: f64::from(rect.origin.y),
        width: f64::from(rect.size.width),
        height: f64::from(rect.size.height),
    }
}

/// Queues a `scroll` event for `target` (`None` = the viewport/document) if
/// the scroll position actually changed; the page's event loop drains the
/// queue and dispatches the events as tasks.
pub(crate) fn note_scroll(cx: &BindCx<'_>, target: Option<NodeId>, changed: bool) {
    if changed {
        cx.state
            .frame
            .pending_scroll_targets
            .borrow_mut()
            .push(target);
    }
}

/// True if `node` is the document element (whose scroll APIs address the
/// viewport per CSSOM-View).
pub(crate) fn is_document_element(dom: &DomTree, node: NodeId) -> bool {
    dom.document_element() == Some(node)
}
