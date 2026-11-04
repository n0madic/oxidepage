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

use std::rc::Rc;

use oxidepage_base::NodeId;
use oxidepage_dom::DomTree;
use oxidepage_layout::LayoutEngine;
use oxidepage_style::Viewport;

use crate::cx::BindCx;
use crate::imp::css_style_declaration::flush_inline_styles;
use crate::state::{FrameShared, RectData};

/// Gives a nested browsing context the viewport of its `<iframe>`'s content
/// box, at `dpr` (the page's).
///
/// The one place the rule lives: the page's pre-order layout walk
/// (ADR-0035 D6) and [`size_frame_to_owner`] both call it. Both engines or
/// neither: the layout engine lays out to the viewport and the stylist's
/// `Device` evaluates media queries against it, so a frame that moved only
/// one answers `@media (max-width: …)` for the size it used to be. An engine
/// already borrowed is left alone (it is being laid out higher up the stack,
/// and that pass sizes it).
pub fn fit_frame_viewport(frame: &FrameShared, content: oxidepage_base::Rect, dpr: f32) {
    let (Ok(mut style), Ok(mut layout)) =
        (frame.style.try_borrow_mut(), frame.layout.try_borrow_mut())
    else {
        return;
    };
    let viewport = Viewport {
        width: content.size.width,
        height: content.size.height,
        dpr,
    };
    if layout.viewport() != viewport {
        layout.set_viewport(viewport);
        style.set_viewport(viewport);
    }
}

/// Sizes a nested browsing context's viewport to its `<iframe>`'s content
/// box, reflowing its ancestors to find that box.
///
/// The page does this in its own pre-order layout walk, but a frame's script
/// can ask for its geometry before the page has walked: a script parsed in
/// the frame's document runs while the page is still loading it. Without
/// this, `innerWidth` and the root's `clientWidth` answer the size the
/// frame's engine was created with, the page's viewport, and its layout wraps
/// to that width too.
pub(crate) fn size_frame_to_owner(cx: &BindCx<'_>, frame: &Rc<FrameShared>) {
    let owner = cx
        .state
        .dom
        .borrow()
        .owner_of_content_document(frame.document());
    let Some(owner) = owner else {
        return; // the top-level frame, whose viewport is the page's
    };
    let parent = cx.frame_for(owner);
    if Rc::ptr_eq(&parent, frame) {
        return;
    }
    size_frame_to_owner(cx, &parent);
    let (content, dpr) = {
        let (Ok(mut style), Ok(mut layout)) = (
            parent.style.try_borrow_mut(),
            parent.layout.try_borrow_mut(),
        ) else {
            return;
        };
        let mut dom = cx.state.dom.borrow_mut();
        // An aborted parent reflow has no boxes, so no content box either.
        let _ = layout.reflow(&mut dom, &mut style);
        // The parent was sized just above, so its dpr is the page's.
        (layout.content_box(owner), layout.viewport().dpr)
    };
    if let Some(content) = content {
        fit_frame_viewport(frame, content, dpr);
    }
}

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
    size_frame_to_owner(cx, &frame);
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
    size_frame_to_owner(cx, &frame);
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
///
/// `scrolled` is the node whose engine moved, and picks the queue: the
/// browsing context rendering it, not the caller's. A parent script scrolling
/// `iframe.contentDocument.documentElement` moves the frame's viewport, so the
/// `scroll` event belongs to the frame's document, and `None` in the parent's
/// queue would fire it at the parent's.
pub(crate) fn note_scroll(
    cx: &BindCx<'_>,
    scrolled: NodeId,
    target: Option<NodeId>,
    changed: bool,
) {
    if changed {
        cx.frame_for(scrolled)
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

/// Whether `node` is the body element of its document (`document.body`).
fn is_the_body_element(dom: &DomTree, node: NodeId) -> bool {
    dom.containing_document(node).is_some_and(|doc| {
        super::document::html_child_of_root_in(dom, doc, &["body", "frameset"]) == Some(node)
    })
}

/// Whether `node`'s document is in quirks mode.
fn in_quirks_mode(dom: &DomTree, node: NodeId) -> bool {
    dom.containing_document(node)
        .is_some_and(|doc| dom.quirks_mode_of(doc) == oxidepage_dom::QuirksMode::Quirks)
}

/// CSSOM-View "potentially scrollable" for the body element: it has a box,
/// and both it and its parent have an `overflow` other than `visible`/`clip`
/// on at least one axis. Read from the *computed* styles, as the spec says:
/// the body's box carries its used overflow after viewport propagation.
pub(crate) fn body_is_potentially_scrollable(
    dom: &DomTree,
    layout: &LayoutEngine,
    body: NodeId,
) -> bool {
    use style::values::specified::box_::Overflow;
    let scrolls = |n: NodeId| {
        dom.primary_style(n).is_some_and(|s| {
            let not_visible = |o: Overflow| !matches!(o, Overflow::Visible | Overflow::Clip);
            not_visible(s.clone_overflow_x()) || not_visible(s.clone_overflow_y())
        })
    };
    layout.tree().box_for_node(body).is_some()
        && dom.get(body).and_then(|n| n.parent()).is_some_and(scrolls)
        && scrolls(body)
}

/// What an element's scroll members (`scrollTop`, `scroll()`, …) address
/// (CSSOM-View §5): the root in standards mode, and the body in quirks mode
/// unless it is potentially scrollable, stand for the viewport; the root in
/// quirks mode scrolls nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollTarget {
    Viewport,
    Element,
    Nothing,
}

pub(crate) fn scroll_target(dom: &DomTree, layout: &LayoutEngine, node: NodeId) -> ScrollTarget {
    let quirks = in_quirks_mode(dom, node);
    if is_document_element(dom, node) {
        return if quirks {
            ScrollTarget::Nothing
        } else {
            ScrollTarget::Viewport
        };
    }
    if quirks
        && is_the_body_element(dom, node)
        && !body_is_potentially_scrollable(dom, layout, node)
    {
        return ScrollTarget::Viewport;
    }
    ScrollTarget::Element
}

/// Whether `clientWidth`/`clientHeight` of `node` report the viewport: the
/// root in standards mode, or the body element in quirks mode.
pub(crate) fn client_is_viewport(dom: &DomTree, node: NodeId) -> bool {
    if in_quirks_mode(dom, node) {
        is_the_body_element(dom, node)
    } else {
        is_document_element(dom, node)
    }
}

/// An element's scroll offset, as its scroll members see it.
pub(crate) fn scroll_offset_of(dom: &DomTree, layout: &LayoutEngine, node: NodeId) -> (f32, f32) {
    match scroll_target(dom, layout, node) {
        ScrollTarget::Viewport => {
            let p = layout.viewport_scroll();
            (p.x, p.y)
        }
        ScrollTarget::Element => {
            let p = layout.scroll_offset(node);
            (p.x, p.y)
        }
        ScrollTarget::Nothing => (0.0, 0.0),
    }
}

/// Sets an element's scroll offset, as its scroll members do. Returns the
/// `note_scroll` target (`None` for the viewport) and whether it moved.
pub(crate) fn set_scroll_offset_of(
    dom: &DomTree,
    layout: &mut LayoutEngine,
    node: NodeId,
    x: f32,
    y: f32,
) -> (Option<NodeId>, bool) {
    match scroll_target(dom, layout, node) {
        ScrollTarget::Viewport => (None, layout.set_viewport_scroll(x, y).changed),
        ScrollTarget::Element => (Some(node), layout.set_scroll_offset(node, x, y).changed),
        ScrollTarget::Nothing => (None, false),
    }
}
