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

/// Flushes styles + layout **of `node`'s browsing context**, then runs `f`
/// with read access to the DOM and that context's laid-out engine.
///
/// The node decides the engine, not the caller's realm: `contentDocument`
/// hands a page's script real nodes of another frame, and measuring those in
/// the caller's engine finds no box at all and answers 0 (ADR-0035 D1).
pub(crate) fn flush_layout<R>(
    cx: &BindCx<'_>,
    node: NodeId,
    f: impl FnOnce(&DomTree, &LayoutEngine) -> R,
) -> R {
    flush_inline_styles(cx);
    let frame = cx.frame_for(node);
    let mut dom = cx.state.dom.borrow_mut();
    let mut style = frame.style.borrow_mut();
    let mut layout = frame.layout.borrow_mut();
    // A geometry getter has no error channel, and inventing half a rectangle
    // from a tree that was thrown away would be exactly the fake P6 forbids.
    // An aborted reflow leaves *no* boxes, so the query below answers what it
    // answers for `display: none` — zeros, and an empty `getClientRects()`.
    // The abort itself is reported through the page (ADR-0037 D6).
    let _ = layout.reflow(&mut dom, &mut style);
    f(&dom, &layout)
}

/// Like [`flush_layout`], but with write access to the layout engine (scroll
/// offset writes).
pub(crate) fn flush_layout_mut<R>(
    cx: &BindCx<'_>,
    node: NodeId,
    f: impl FnOnce(&DomTree, &mut LayoutEngine) -> R,
) -> R {
    flush_inline_styles(cx);
    let frame = cx.frame_for(node);
    let mut dom = cx.state.dom.borrow_mut();
    let mut style = frame.style.borrow_mut();
    let mut layout = frame.layout.borrow_mut();
    // Zeros on an aborted reflow, as in `flush_layout` above.
    let _ = layout.reflow(&mut dom, &mut style);
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
    // Its *own* document's root: a frame's `<html>` addresses the frame's
    // viewport, exactly as the page's addresses the page's.
    dom.containing_document(node)
        .and_then(|doc| dom.document_element_of(doc))
        == Some(node)
}
