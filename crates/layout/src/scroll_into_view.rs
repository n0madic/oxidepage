//! Revealing an element: the scroll algorithm behind `Element.scrollIntoView`
//! and `Page::scroll_into_view_if_needed`.
//!
//! It lives here, not in `bindings`, for the reason [`crate::transform`] and
//! [`crate::multicol::map_flow_point`] give: the embedder-facing primitive and
//! the Web API must scroll to the *same* place, and two implementations of
//! "align nearest on both axes" would eventually be two answers. Both callers
//! queue `scroll` events from the target list this returns.
//!
//! **This must never re-enter JS** (CLAUDE.md): it reads layout and writes
//! scroll offsets under the caller's borrows. Events are the caller's job,
//! after it returns.

use oxidepage_base::{NodeId, Point, Rect};
use oxidepage_dom::DomTree;

use crate::engine::LayoutEngine;
use crate::geometry::ScrollParent;

/// `block`/`inline` alignment (CSSOM-View `ScrollLogicalPosition`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Start,
    Center,
    End,
    /// Scroll the minimum needed — and nothing at all if the element is already
    /// fully visible. This is the default for both axes, and the reason a
    /// `scrollIntoView()` on something already on screen is a no-op. It is also
    /// the whole of "scroll into view *if needed*".
    Nearest,
}

/// Scrolls every scroll container on `element`'s containing-block chain,
/// innermost first, so an element nested in a scroll container inside the
/// document ends up visible in **both** — scrolling only the nearest one leaves
/// it off-screen, which is the bug an automation driver hits immediately. Each
/// step re-reads the element's rect, because the previous scroll moved it.
///
/// `reveal` is an optional sub-rectangle to bring into view, relative to the
/// element's border-box origin (CDP's `DOM.scrollIntoViewIfNeeded`); `None`
/// reveals the whole element. It is offset within the element's *visual*
/// bounding rect, so on a rotated element it is an approximation — as is
/// everything measured in axis-aligned rects there.
///
/// Returns the scroll targets whose offset actually changed, in the order they
/// were scrolled (`None` = the viewport). The caller queues one `scroll` event
/// per entry.
///
/// **`behavior: "smooth"` is treated as instant** by both callers, and that is a
/// documented limit: there is no animation timeline here, and a driver wants the
/// final position anyway.
pub fn scroll_into_view(
    layout: &mut LayoutEngine,
    dom: &DomTree,
    element: NodeId,
    reveal: Option<Rect>,
    block: Align,
    inline: Align,
) -> Vec<Option<NodeId>> {
    let mut changed = Vec::new();

    // `node` walks up the chain; the element being revealed stays `element`.
    let mut node = element;
    loop {
        match layout.scroll_parent(dom, node) {
            ScrollParent::None => return changed,
            ScrollParent::DocumentScrollingElement => {
                let Some(rect) = reveal_rect(layout, element, reveal) else {
                    return changed;
                };
                let viewport = layout.viewport();
                let offset = layout.viewport_scroll();
                let (x, y) =
                    aligned_offset(rect, viewport.width, viewport.height, offset, block, inline);
                if layout.set_viewport_scroll(x, y).changed {
                    changed.push(None);
                }
                return changed;
            }
            ScrollParent::Element(container) => {
                if let (Some(rect), Some(view)) = (
                    reveal_rect(layout, element, reveal),
                    layout.padding_box(container),
                ) {
                    let offset = layout.scroll_offset(container);
                    // A scroll offset is in the container's **own** content px,
                    // so both rects come back out of visual space first — under
                    // a transformed ancestor the visual delta is the scaled one,
                    // and scrolling by it overshoots (ADR-0026).
                    let rect = layout.unmap_into_scroll_space(container, rect);
                    let view = layout.unmap_into_scroll_space(container, view);
                    // `border_box`/`padding_box` are both *viewport*-relative,
                    // so their difference is already the visual delta from the
                    // container's near edge — the position `axis_offset` wants.
                    // Adding `offset` too counted the container's current scroll
                    // twice, which made a second `scrollIntoView()` (a no-op in
                    // a browser) scroll the container by another full delta and
                    // made `Align::Nearest`'s "above the top" test unreachable.
                    let local = Rect::from_xywh(
                        rect.origin.x - view.origin.x,
                        rect.origin.y - view.origin.y,
                        rect.size.width,
                        rect.size.height,
                    );
                    let (x, y) = aligned_offset(
                        local,
                        view.size.width,
                        view.size.height,
                        offset,
                        block,
                        inline,
                    );
                    if layout.set_scroll_offset(container, x, y).changed {
                        changed.push(Some(container));
                    }
                }
                node = container;
            }
        }
    }
}

/// The rect to bring into view, in viewport coordinates: the element's border
/// box, or `reveal` placed within it.
fn reveal_rect(layout: &LayoutEngine, element: NodeId, reveal: Option<Rect>) -> Option<Rect> {
    let border = layout.border_box(element)?;
    Some(match reveal {
        None => border,
        Some(sub) => Rect::from_xywh(
            border.origin.x + sub.origin.x,
            border.origin.y + sub.origin.y,
            sub.size.width,
            sub.size.height,
        ),
    })
}

/// The scroll offset that brings `rect` (relative to the scrollport's near
/// edge) into a `view_w` × `view_h` scrollport currently scrolled to `current`.
fn aligned_offset(
    rect: Rect,
    view_w: f32,
    view_h: f32,
    current: Point,
    block: Align,
    inline: Align,
) -> (f32, f32) {
    // The block axis is vertical and the inline axis horizontal for the
    // horizontal-tb writing mode, which is the only one laid out here.
    let y = axis_offset(rect.origin.y, rect.size.height, view_h, current.y, block);
    let x = axis_offset(rect.origin.x, rect.size.width, view_w, current.x, inline);
    (x, y)
}

fn axis_offset(start: f32, size: f32, view: f32, current: f32, align: Align) -> f32 {
    match align {
        Align::Start => current + start,
        Align::Center => current + start - (view - size) / 2.0,
        Align::End => current + start - (view - size),
        Align::Nearest => {
            if start < 0.0 {
                // Off the near edge: bring that edge into view.
                current + start
            } else if start + size > view {
                // Off the far edge: scroll the minimum that shows it, but never
                // past the near edge for a box taller than the viewport.
                current + (start + size - view).min(start)
            } else {
                current
            }
        }
    }
}
