//! Post-layout scrollable-overflow pass (adapted from blitz-dom
//! `resolve_transforms`, without transforms: geometry ignores CSS transforms
//! in v1, ADR-0006 §6).
//!
//! For every box we store the union of its own **padding box** and all
//! descendants' overflow contributions in its own coordinate space
//! (`scrollable_overflow`, feeding `scrollWidth`/`scrollHeight`). Padding box,
//! not border box, is what CSS Overflow §3.2 seeds the scrollable overflow area
//! with — seeding it with the border box makes `scrollHeight` report
//! `clientHeight + border-bottom-width` for every bordered box.
//!
//! What a box contributes *to its parent* is a different rect: the parent's
//! overflow area has to cover this box's whole **border** box (a border sticking
//! out of the parent still overflows it), unioned with this box's own scrollable
//! overflow on each axis where this box does not clip (a scroll container hides
//! its inner overflow from the outside).

use oxidepage_base::geometry::Rect;
use style::Atom;
use taffy::Overflow;

use crate::tree::{BoxId, LayoutTree};

/// Runs the overflow pass over the whole tree.
pub(crate) fn resolve_scrollable_overflow(tree: &mut LayoutTree) {
    if let Some(root) = tree.root() {
        resolve_recursive(tree, root);
    }
}

/// Which physical direction is the *logical end* on each axis, for the
/// purpose of scrollable-overflow accounting (CSS Overflow §3.2 distinguishes
/// start-ward overflow, which scrolling can never reach, from end-ward
/// overflow, which always counts). The block axis is always physical
/// top→bottom in this project's horizontal-writing-mode-only scope (ADR-0006
/// §7), so only `flex-direction: column-reverse` can flip it. The inline axis
/// flips with `direction: rtl`, and — independently — with
/// `flex-direction: row-reverse`, which redefines the flex *main-start* to
/// the inline-end without touching `direction` itself; when both flip, they
/// cancel out. `flex-wrap: wrap-reverse` reorders cross-axis lines but does
/// not redefine which physical edge is the axis's end, so it is not
/// considered here.
pub(crate) fn logical_end_is_positive(style: &taffy::Style<Atom>) -> (bool, bool) {
    let rtl = taffy::CoreStyle::direction(style) == taffy::Direction::Rtl;
    if style.display != taffy::Display::Flex {
        return (!rtl, true);
    }
    match style.flex_direction {
        taffy::FlexDirection::Row => (!rtl, true),
        taffy::FlexDirection::RowReverse => (rtl, true),
        taffy::FlexDirection::Column => (!rtl, true),
        taffy::FlexDirection::ColumnReverse => (!rtl, false),
    }
}

/// Computes `scrollable_overflow` for `box_id` and returns its contribution
/// to the parent's overflow, in the parent's coordinate space.
fn resolve_recursive(tree: &mut LayoutTree, box_id: BoxId) -> Rect {
    let layout = tree.box_(box_id).final_layout;
    let border_box = Rect::from_xywh(0.0, 0.0, layout.size.width, layout.size.height);
    let padding_box = Rect::from_xywh(
        layout.border.left,
        layout.border.top,
        (layout.size.width - layout.border.left - layout.border.right).max(0.0),
        (layout.size.height - layout.border.top - layout.border.bottom).max(0.0),
    );
    let content_box = Rect::from_xywh(
        layout.border.left + layout.padding.left,
        layout.border.top + layout.padding.top,
        (layout.size.width
            - layout.border.left
            - layout.border.right
            - layout.padding.left
            - layout.padding.right)
            .max(0.0),
        (layout.size.height
            - layout.border.top
            - layout.border.bottom
            - layout.padding.top
            - layout.padding.bottom)
            .max(0.0),
    );

    // A multicol root shows its (much taller) flow child through per-column
    // clipped views: the flow's height is *columns*, not overflow. Recurse so
    // descendants still get their own `scrollable_overflow` (for a `scrollHeight`
    // query on one of them), but drop the flow's contribution — otherwise the
    // document's content size, and with it the page height and the maximum
    // viewport scroll, would be the un-columnized height.
    let is_multicol = tree.box_(box_id).multicol.is_some();
    let (x_end_positive, y_end_positive) = logical_end_is_positive(&tree.box_(box_id).style);

    // Children are accumulated separately from `padding_box` so the trailing-
    // padding bonus below can compare their *raw* extent against the content
    // box, rather than against a union that padding_box may already dominate.
    let mut children_extent = Rect::ZERO;
    // The trailing-padding bonus below is triggered by whichever child
    // extends furthest on the logical-end edge — but a child's *own*
    // negative margin can push its border box past the content edge purely
    // as an artifact of the auto-size formula (e.g. `margin: -7px` widening
    // an auto-width block past its containing block), and CSS Overflow
    // excludes that margin-driven overhang from scrollable overflow
    // (`scrollWidthHeight-overflow-visible-negative-margins.html`). Tracked
    // per child and per the specific edge being tested (only the margin
    // component on *that* edge, e.g. `margin-right` for the positive-x
    // trigger), not a container-wide flag — otherwise one child's negative
    // margin would suppress a *different* child's genuine end-ward overflow
    // from triggering the bonus at all.
    let (mut trigger_max_x, mut trigger_min_x) = (f32::NEG_INFINITY, f32::INFINITY);
    let (mut trigger_max_y, mut trigger_min_y) = (f32::NEG_INFINITY, f32::INFINITY);
    // Indexed iteration: `resolve_recursive` needs `&mut tree`, so the
    // children list cannot stay borrowed (and cloning it would be one heap
    // allocation per box per reflow).
    let mut index = 0;
    while let Some(&child) = tree.box_(box_id).children.get(index) {
        index += 1;
        // An outside list marker hangs off the item's start edge by design. CSS
        // clips the scrollable overflow region at that edge, so the marker
        // contributes none: without this a `<li>` would report a `scrollWidth`
        // wider than its `clientWidth` for every bullet on the page.
        let is_marker = tree.box_(child).is_outside_marker();
        let contribution = resolve_recursive(tree, child);
        if !is_multicol && !is_marker {
            children_extent = children_extent.union(&contribution);
            let m = tree.box_(child).final_layout.margin;
            if x_end_positive {
                if m.right >= 0.0 {
                    trigger_max_x = trigger_max_x.max(contribution.max_x());
                }
            } else if m.left >= 0.0 {
                trigger_min_x = trigger_min_x.min(contribution.min_x());
            }
            if y_end_positive {
                if m.bottom >= 0.0 {
                    trigger_max_y = trigger_max_y.max(contribution.max_y());
                }
            } else if m.top >= 0.0 {
                trigger_min_y = trigger_min_y.min(contribution.min_y());
            }
        }
    }

    // CSS Overflow §3.2's "trailing padding" rule: when content genuinely
    // overflows past the box's own *content* edge on its logical-end side,
    // the scrollable region extends one more padding-width past it, so
    // scrolling all the way to the end still shows the full end padding
    // (mirrors every non-overflowing box, whose padding is already included
    // via `padding_box`). Only the end side gets this — the start side's
    // negative excess is deliberately left for `scroll_size()` to ignore.
    // Triggered by `trigger_max_x`/`trigger_min_x`/`trigger_max_y`/
    // `trigger_min_y` above; the bonus amount itself still extends the full
    // `children_extent`, since a triggering child's margin is non-negative
    // on the tested edge by construction.
    let mut overflow = padding_box;
    if !children_extent.is_empty() {
        let mut bonused = children_extent;
        if x_end_positive {
            if trigger_max_x > content_box.max_x() {
                bonused = Rect::from_xywh(
                    bonused.min_x(),
                    bonused.min_y(),
                    bonused.size.width + layout.padding.right,
                    bonused.size.height,
                );
            }
        } else if trigger_min_x < content_box.min_x() {
            let extended_min_x = bonused.min_x() - layout.padding.left;
            bonused = Rect::from_xywh(
                extended_min_x,
                bonused.min_y(),
                bonused.max_x() - extended_min_x,
                bonused.size.height,
            );
        }
        if y_end_positive {
            if trigger_max_y > content_box.max_y() {
                bonused = Rect::from_xywh(
                    bonused.min_x(),
                    bonused.min_y(),
                    bonused.size.width,
                    bonused.size.height + layout.padding.bottom,
                );
            }
        } else if trigger_min_y < content_box.min_y() {
            let extended_min_y = bonused.min_y() - layout.padding.top;
            bonused = Rect::from_xywh(
                bonused.min_x(),
                extended_min_y,
                bonused.size.width,
                bonused.max_y() - extended_min_y,
            );
        }
        overflow = overflow.union(&bonused);
    }

    let this = tree.box_mut(box_id);
    this.scrollable_overflow = overflow;

    // What the parent sees: always at least this box's border box, plus the
    // scrollable overflow on each axis this box does not clip.
    let unclipped = border_box.union(&overflow);
    let clip_x = this.style.overflow.x != Overflow::Visible;
    let clip_y = this.style.overflow.y != Overflow::Visible;
    let contribution = Rect {
        origin: oxidepage_base::Point {
            x: if clip_x {
                border_box.origin.x
            } else {
                unclipped.origin.x
            },
            y: if clip_y {
                border_box.origin.y
            } else {
                unclipped.origin.y
            },
        },
        size: oxidepage_base::Size {
            width: if clip_x {
                border_box.size.width
            } else {
                unclipped.size.width
            },
            height: if clip_y {
                border_box.size.height
            } else {
                unclipped.size.height
            },
        },
    };

    contribution.translate(layout.location.x, layout.location.y)
}
