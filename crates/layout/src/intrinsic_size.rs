//! Pre-layout pass resolving `min-content`/`max-content` size keywords
//! (`crate::tree::IntrinsicSizeKeyword`, flagged at construction time by
//! `construct::intrinsic_size_keywords_for`) to concrete pixel `Dimension`s.
//!
//! Taffy has no per-call hook for a keyword-driven size: `get_core_container_style`/
//! `get_flexbox_child_style`/`get_grid_child_style` all return a reference to
//! the style physically stored on the box, and its built-in layout algorithms
//! read `Dimension` values straight off it. So the only way to make taffy see
//! a min-content/max-content-derived size is to measure it ourselves and
//! overwrite the stored style before its real layout pass runs.
//!
//! This pass is writing-mode-blind, like the rest of layout (ADR-0006 §7):
//! `writing-mode` never reaches `taffy_impl`/`construct`, only `getComputedStyle`'s
//! logical→physical property mapping (`style::computed::serialize_property`).
//! A `min-content`/`max-content` box with `writing-mode: vertical-rl` is thus
//! measured identically to one without it — correctly fixing the horizontal-tb
//! case for such content can flip a `vertical-rl` WPT case that previously
//! (and only coincidentally) matched by getting the *same* physically-blind
//! answer the spec happens to expect for a different, unimplemented reason.
//! That is expected collateral of the v1 scope decision, not a fixable
//! regression in this pass.

use taffy::prelude::TaffyAuto as _;
use taffy::{
    AvailableSpace, BoxSizing, CoreStyle as _, LayoutInput, LayoutPartialTree as _, RequestedAxis,
    ResolveOrZero as _, RunMode, Size, SizingMode,
};

use crate::taffy_impl::resolve_calc_value;
use crate::tree::{BoxId, IntrinsicSizeKeyword, IntrinsicSizeTarget, LayoutTree};

/// Runs the pass over the whole tree, in true post-order: a parent's own
/// content-size measurement must see its children's already-resolved
/// concrete sizes, not their still-keyword-driven ones.
pub(crate) fn resolve_intrinsic_size_keywords(tree: &mut LayoutTree) {
    if let Some(root) = tree.root() {
        resolve_recursive(tree, root);
    }
}

fn resolve_recursive(tree: &mut LayoutTree, box_id: BoxId) {
    let children = tree.box_(box_id).children.clone();
    for child in children {
        resolve_recursive(tree, child);
    }

    if tree.box_(box_id).intrinsic_size_keywords.is_empty() {
        return;
    }
    let keywords = tree.box_(box_id).intrinsic_size_keywords.clone();
    for (target, keyword) in keywords {
        let available = match keyword {
            IntrinsicSizeKeyword::MinContent => AvailableSpace::MinContent,
            IntrinsicSizeKeyword::MaxContent => AvailableSpace::MaxContent,
        };
        let is_width = match target {
            IntrinsicSizeTarget::Width
            | IntrinsicSizeTarget::MinWidth
            | IntrinsicSizeTarget::MaxWidth => true,
            IntrinsicSizeTarget::Height
            | IntrinsicSizeTarget::MinHeight
            | IntrinsicSizeTarget::MaxHeight => false,
            IntrinsicSizeTarget::FlexBasis => {
                // flex-basis's axis is the containing flex container's main
                // axis: row/row-reverse → width, column/column-reverse →
                // height. A basis on a box no longer in a flex container
                // (e.g. detached mid-mutation) has nothing to resolve against.
                let Some(parent) = tree.box_(box_id).parent else {
                    continue;
                };
                let is_column = matches!(
                    tree.box_(parent).style.flex_direction,
                    taffy::FlexDirection::Column | taffy::FlexDirection::ColumnReverse
                );
                // A column main axis needs the item's own *cross*-axis
                // (width) size first — typically stretched to the
                // container's cross size by the flex algorithm this pass
                // runs ahead of — to correctly measure wrapping content's
                // block-axis min/max-content size (e.g. floats that wrap
                // once stretched to their real width). That's a genuine
                // circular dependency this pass can't resolve; leave
                // `flex_basis` on `AUTO`, which already falls back to the
                // normal auto-height-with-stretched-width block algorithm —
                // coincidentally correct for content whose intrinsic block
                // size doesn't itself depend on the *inline* axis.
                if is_column {
                    continue;
                }
                true
            }
        };
        // Reset the target field to `AUTO` — and clear the cache — before
        // measuring. Otherwise this box's *own* previously-resolved value
        // (written by this same pass on an earlier reflow) acts as a
        // self-referential floor/ceiling: taffy clamps final size against
        // min/max regardless of sizing mode, so a stale `min_size.width` from
        // when the content was wider would clamp *this* measurement back up
        // to it even though the content has since shrunk — a ratchet that
        // only ever grows. The cache is keyed by `LayoutInput`, which is
        // identical across reflows for these synthetic measurement inputs,
        // so it can't tell this reset apart from the last call without help.
        {
            let style = &mut tree.box_mut(box_id).style;
            match target {
                IntrinsicSizeTarget::Width => style.size.width = taffy::Dimension::AUTO,
                IntrinsicSizeTarget::Height => style.size.height = taffy::Dimension::AUTO,
                IntrinsicSizeTarget::MinWidth => style.min_size.width = taffy::Dimension::AUTO,
                IntrinsicSizeTarget::MinHeight => style.min_size.height = taffy::Dimension::AUTO,
                IntrinsicSizeTarget::MaxWidth => style.max_size.width = taffy::Dimension::AUTO,
                IntrinsicSizeTarget::MaxHeight => style.max_size.height = taffy::Dimension::AUTO,
                IntrinsicSizeTarget::FlexBasis => style.flex_basis = taffy::Dimension::AUTO,
            }
        }
        tree.box_mut(box_id).cache.clear();

        // `SizingMode::ContentSize`, not `InherentSize`: `InherentSize` tells
        // taffy to prefer the node's own explicit inherent size (its
        // still-keyword `width`/`flex-basis` itself, or an unrelated sibling
        // property like an explicit `width` alongside `flex-basis:
        // max-content`) over content contributions — exactly the value this
        // pass exists to ignore and replace with the true, content-only
        // min-content/max-content size. The other axis stays unconstrained
        // (`MaxContent`) unless it's separately flagged too, in which case
        // its own iteration resolves it.
        let (axis, available_space) = if is_width {
            (
                RequestedAxis::Horizontal,
                Size {
                    width: available,
                    height: AvailableSpace::MaxContent,
                },
            )
        } else {
            (
                RequestedAxis::Vertical,
                Size {
                    width: AvailableSpace::MaxContent,
                    height: available,
                },
            )
        };
        let output = tree.compute_child_layout(
            box_id.into(),
            LayoutInput {
                run_mode: RunMode::ComputeSize,
                sizing_mode: SizingMode::ContentSize,
                axis,
                known_dimensions: Size::NONE,
                parent_size: Size::NONE,
                available_space,
                vertical_margins_are_collapsible: taffy::Line::FALSE,
            },
        );
        // `LayoutOutput::size` is always the *border-box* size, but a
        // `Dimension`-typed style field (`width`, `min-height`, `flex-basis`,
        // …) is interpreted content-box-relative unless `box-sizing:
        // border-box` — matching taffy's own `box_sizing_adjustment` in
        // `determine_flex_base_size`. Skipping this double-counts border and
        // padding on every resolved box that has any (`flex-minimum-height-
        // flex-items-031.html`'s 2px border caught this).
        let (box_sizing, border, padding) = {
            let style = &tree.box_(box_id).style;
            (style.box_sizing, style.border(), style.padding())
        };
        let border_padding = if box_sizing == BoxSizing::ContentBox {
            let border = border.resolve_or_zero(None, resolve_calc_value);
            let padding = padding.resolve_or_zero(None, resolve_calc_value);
            if is_width {
                border.horizontal_axis_sum() + padding.horizontal_axis_sum()
            } else {
                border.vertical_axis_sum() + padding.vertical_axis_sum()
            }
        } else {
            0.0
        };
        let px = (if is_width {
            output.size.width
        } else {
            output.size.height
        } - border_padding)
            .max(0.0);
        let style = &mut tree.box_mut(box_id).style;
        match target {
            IntrinsicSizeTarget::Width => style.size.width = taffy::Dimension::length(px),
            IntrinsicSizeTarget::Height => style.size.height = taffy::Dimension::length(px),
            IntrinsicSizeTarget::MinWidth => style.min_size.width = taffy::Dimension::length(px),
            IntrinsicSizeTarget::MinHeight => {
                style.min_size.height = taffy::Dimension::length(px);
            }
            IntrinsicSizeTarget::MaxWidth => style.max_size.width = taffy::Dimension::length(px),
            IntrinsicSizeTarget::MaxHeight => {
                style.max_size.height = taffy::Dimension::length(px);
            }
            IntrinsicSizeTarget::FlexBasis => style.flex_basis = taffy::Dimension::length(px),
        }
    }
}
