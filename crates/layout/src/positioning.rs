//! Out-of-flow positioning: gives absolutely positioned boxes the containing
//! block CSS asks for.
//!
//! Taffy has no containing-block chain — it lays an `Absolute` child out
//! against its *direct parent*, which is what ADR-0006 §5 shipped in v1. CSS
//! instead resolves `position: absolute` against the nearest *positioned*
//! ancestor (and `position: fixed` against the viewport), so any page that
//! nests an absolutely positioned element deeper than its `position: relative`
//! wrapper — an overlay, a dropdown, a decorative underline — lands in the
//! wrong place, usually on top of its neighbours.
//!
//! [`hoist_out_of_flow`] therefore re-parents each out-of-flow box onto its
//! real containing block *before* layout, so taffy resolves both its insets and
//! its percentage sizes against that box. A box whose containing block already
//! is its parent (the overwhelmingly common `position: relative` wrapper) is
//! left exactly where it was.
//!
//! Hoisting loses one thing taffy gave us for free: the *static position* — the
//! place a box with `auto` insets would have taken in the flow, which taffy
//! approximates with its parent's content-box origin. [`restore_static_positions`]
//! puts it back after layout, on each axis whose insets are both `auto`.

use oxidepage_dom::DomTree;
use taffy::Position;

use crate::tree::{BoxId, LayoutTree};

/// True when `box_id` has a non-`none` `transform`. Per CSS a transformed box
/// establishes a containing block for both absolute *and* fixed descendants
/// (paint applies the transform to them, so layout must resolve them against
/// that box too, or the layout containing block and paint parentage disagree).
/// Anonymous boxes carry no DOM node and cannot be transformed.
fn has_transform(tree: &LayoutTree, dom: &DomTree, box_id: BoxId) -> bool {
    tree.box_(box_id)
        .dom_node
        .and_then(|node| dom.primary_style(node))
        .is_some_and(|style| !style.get_box().transform.0.is_empty())
}

/// Whether `box_id` establishes the containing block an out-of-flow box with
/// the given positioning scheme resolves against. An `absolute` box is
/// contained by the nearest ancestor that is positioned (any `position` other
/// than `static`) *or* transformed; a `fixed` box only by a *transformed*
/// ancestor — a merely `relative`/`absolute` ancestor does not capture it, so
/// it stays fixed to the viewport. (Taffy's own style collapses
/// `static`/`relative`/`sticky`, so the position is read from the stylo value
/// the box tree keeps alongside it.)
fn establishes_containing_block(
    tree: &LayoutTree,
    dom: &DomTree,
    box_id: BoxId,
    fixed: bool,
) -> bool {
    // A multicol *flow* box contains every out-of-flow descendant, `fixed`
    // included (ADR-0016). Paint shows the flow through per-column clip +
    // translate views, and reaches a hoisted box through the static parent it
    // was built under — which is *inside* the flow. A box whose position was
    // resolved against a containing block outside the flow would still be
    // painted from in there, and the column transform would be applied to a
    // coordinate that never had it. Keeping every box under a multicol root in
    // one coordinate space is what lets paint, geometry and hit-testing share a
    // single mapping rule.
    if tree.multicol_root_of_flow(box_id).is_some() {
        return true;
    }
    if fixed {
        has_transform(tree, dom, box_id)
    } else {
        tree.box_(box_id).position != style::computed_values::position::T::Static
            || has_transform(tree, dom, box_id)
    }
}

/// The containing block for an out-of-flow box: the nearest ancestor that
/// establishes one for its positioning scheme (see
/// [`establishes_containing_block`]), falling back to the root (viewport).
fn containing_block(tree: &LayoutTree, dom: &DomTree, box_id: BoxId, root: BoxId) -> BoxId {
    let fixed = tree.box_(box_id).position == style::computed_values::position::T::Fixed;
    let mut ancestor = tree.box_(box_id).parent;
    while let Some(current) = ancestor {
        if current == root || establishes_containing_block(tree, dom, current, fixed) {
            return current;
        }
        ancestor = tree.box_(current).parent;
    }
    root
}

/// Re-parents every out-of-flow box onto its CSS containing block. Runs once
/// per box-tree build, before the first layout pass.
pub(crate) fn hoist_out_of_flow(tree: &mut LayoutTree, dom: &DomTree) {
    let Some(root) = tree.root() else { return };

    for index in 0..tree.boxes.len() {
        let box_id = BoxId(index as u32);
        if box_id == root {
            continue;
        }
        if tree.box_(box_id).style.position != Position::Absolute {
            continue;
        }
        // An outside list marker is `absolute` only to stay out of its item's
        // flow — CSS does not position it against a containing block, and
        // `marker::place_markers` places it relative to the item itself. Hoisting
        // it onto some positioned ancestor would take that anchor away.
        if tree.box_(box_id).is_outside_marker() {
            continue;
        }
        let Some(parent) = tree.box_(box_id).parent else {
            continue;
        };
        let target = containing_block(tree, dom, box_id, root);
        if target == parent {
            continue;
        }

        tree.box_mut(parent).children.retain(|&c| c != box_id);
        tree.box_mut(parent).hoisted_children.push(box_id);
        tree.box_mut(target).children.push(box_id);
        let hoisted = tree.box_mut(box_id);
        hoisted.parent = Some(target);
        hoisted.static_parent = Some(parent);
    }
}

/// This box's border-box origin in the root's coordinate space.
fn absolute_origin(tree: &LayoutTree, box_id: BoxId) -> taffy::Point<f32> {
    let mut origin = taffy::Point::ZERO;
    let mut current = Some(box_id);
    while let Some(id) = current {
        let b = tree.box_(id);
        origin.x += b.unrounded_layout.location.x;
        origin.y += b.unrounded_layout.location.y;
        current = b.parent;
    }
    origin
}

/// The content-box origin (border + padding inside the border box), absolute.
fn absolute_content_origin(tree: &LayoutTree, box_id: BoxId) -> taffy::Point<f32> {
    let origin = absolute_origin(tree, box_id);
    let layout = &tree.box_(box_id).unrounded_layout;
    taffy::Point {
        x: origin.x + layout.border.left + layout.padding.left,
        y: origin.y + layout.border.top + layout.padding.top,
    }
}

/// Puts the static position back for hoisted boxes: on each axis where both
/// insets are `auto`, CSS places the box where it would have sat in the flow —
/// i.e. at the content origin of the parent it was *built* under, not of the
/// containing block taffy laid it out against. The box is moved there outright
/// (rather than by a relative nudge) because taffy's own placement for auto
/// insets is an approximation we are replacing, not a value to build on.
///
/// Runs after taffy's layout pass and before rounding, in the same
/// `post_layout_offset` regime as the float corrections (so the next
/// incremental reflow removes it before recomputing).
pub(crate) fn restore_static_positions(tree: &mut LayoutTree) {
    // Boxes are stored in build order, so every box precedes its descendants
    // and its containing block (both ancestors built earlier). Applying each
    // correction immediately, in that order, means a hoisted box whose
    // `static_parent`/containing-block chain runs through another corrected
    // box reads its already-corrected coordinates rather than stale ones.
    for index in 0..tree.boxes.len() {
        let box_id = BoxId(index as u32);
        let Some(static_parent) = tree.box_(box_id).static_parent else {
            continue;
        };
        let Some(parent) = tree.box_(box_id).parent else {
            continue;
        };

        let inset = &tree.box_(box_id).style.inset;
        let auto_x = inset.left.is_auto() && inset.right.is_auto();
        let auto_y = inset.top.is_auto() && inset.bottom.is_auto();
        if !auto_x && !auto_y {
            continue;
        }

        // Locations are relative to the containing block's border box, and
        // taffy has already offset the box by its own margin.
        let static_origin = absolute_content_origin(tree, static_parent);
        let cb_origin = absolute_origin(tree, parent);
        let layout = &tree.box_(box_id).unrounded_layout;
        let margin = layout.margin;
        let current = layout.location;

        let delta = taffy::Point {
            x: if auto_x {
                static_origin.x - cb_origin.x + margin.left - current.x
            } else {
                0.0
            },
            y: if auto_y {
                static_origin.y - cb_origin.y + margin.top - current.y
            } else {
                0.0
            },
        };
        if delta.x != 0.0 || delta.y != 0.0 {
            let b = tree.box_mut(box_id);
            b.unrounded_layout.location.x += delta.x;
            b.unrounded_layout.location.y += delta.y;
            b.post_layout_offset.x += delta.x;
            b.post_layout_offset.y += delta.y;
        }
    }
}
