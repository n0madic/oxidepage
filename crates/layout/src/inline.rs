//! The taffy ↔ parley seam: laying out an inline formatting context inside
//! taffy's box layout (adapted from blitz-dom `layout/inline.rs`, minus the
//! `floats` feature and with `scale` fixed at 1.0 — everything is CSS px,
//! ADR-0006).
//!
//! The parley layout is `take`n out of the box and put back at the end so
//! `&mut self` stays available for measuring atomic inline children.

use parley::{AlignmentOptions, IndentOptions};
use style::values::computed::CSSPixelLength;
use taffy::{
    AvailableSpace, BlockContext, BlockFormattingContext, BoxSizing, CollapsibleMarginSet,
    CoreStyle as _, LayoutInput, LayoutOutput, LayoutPartialTree as _, MaybeMath as _,
    MaybeResolve as _, NodeId, Overflow, Point, Position, ResolveOrZero as _, RunMode, Size,
    SizingMode,
};

use crate::taffy_impl::resolve_calc_value;
use crate::tree::{BoxId, LayoutTree};

/// The node's resolved size constraints:
/// `(node_size, min_size, max_size, aspect_ratio)`.
type ResolvedSizes = (
    Size<Option<f32>>,
    Size<Option<f32>>,
    Size<Option<f32>>,
    Option<f32>,
);

/// Resolves the node's preferred/min/max sizes against the parent size
/// (percentages become pixels). For `ContentSize` mode the node's size
/// styles are ignored, per taffy's sizing-mode contract.
pub(crate) fn resolve_node_sizes(
    style: &taffy::Style<style::Atom>,
    known_dimensions: Size<Option<f32>>,
    parent_size: Size<Option<f32>>,
    sizing_mode: SizingMode,
    box_sizing_adjustment: Size<f32>,
) -> ResolvedSizes {
    match sizing_mode {
        SizingMode::ContentSize => (known_dimensions, Size::NONE, Size::NONE, None),
        SizingMode::InherentSize => {
            let aspect_ratio = style.aspect_ratio();
            let style_size = style
                .size()
                .maybe_resolve(parent_size, resolve_calc_value)
                .maybe_apply_aspect_ratio(aspect_ratio)
                .maybe_add(box_sizing_adjustment);
            let style_min_size = style
                .min_size()
                .maybe_resolve(parent_size, resolve_calc_value)
                .maybe_apply_aspect_ratio(aspect_ratio)
                .maybe_add(box_sizing_adjustment);
            let style_max_size = style
                .max_size()
                .maybe_resolve(parent_size, resolve_calc_value)
                .maybe_add(box_sizing_adjustment);

            let node_size =
                known_dimensions.or(style_size.maybe_clamp(style_min_size, style_max_size));
            (node_size, style_min_size, style_max_size, aspect_ratio)
        }
    }
}

impl LayoutTree {
    pub(crate) fn compute_inline_layout(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> taffy::LayoutOutput {
        let LayoutInput {
            known_dimensions,
            parent_size,
            run_mode,
            ..
        } = inputs;
        let style = &self.box_(BoxId::from(node_id)).style;

        // Pull these out earlier to avoid borrowing issues
        let is_scroll_container =
            style.overflow.x.is_scroll_container() || style.overflow.y.is_scroll_container();
        let padding = style
            .padding()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let border = style
            .border()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let padding_border_size = (padding + border).sum_axes();
        let box_sizing_adjustment = if style.box_sizing() == BoxSizing::ContentBox {
            padding_border_size
        } else {
            Size::ZERO
        };

        let (clamped_style_size, min_size, max_size, _aspect_ratio) = resolve_node_sizes(
            style,
            known_dimensions,
            parent_size,
            inputs.sizing_mode,
            box_sizing_adjustment,
        );

        // If both min and max in a given axis are set and max <= min then
        // this determines the size in that axis
        let min_max_definite_size = min_size.zip_map(max_size, |min, max| match (min, max) {
            (Some(min), Some(max)) if max <= min => Some(min),
            _ => None,
        });

        let styled_based_known_dimensions = known_dimensions
            .or(min_max_definite_size)
            .or(clamped_style_size)
            .maybe_max(padding_border_size);

        // Short-circuit layout if the container's size is fully determined by
        // the container's size and the run mode is ComputeSize (and thus the
        // container's size is all that we're interested in)
        if run_mode == RunMode::ComputeSize
            && let Size {
                width: Some(width),
                height: Some(height),
            } = styled_based_known_dimensions
        {
            return LayoutOutput::from_outer_size(Size { width, height });
        }

        // Unwrap the block formatting context if one was passed, or else
        // create a new one
        match block_ctx {
            Some(inherited_bfc) if !is_scroll_container => self.compute_inline_layout_inner(
                node_id,
                LayoutInput {
                    known_dimensions: styled_based_known_dimensions,
                    ..inputs
                },
                inherited_bfc,
            ),
            _ => {
                let mut root_bfc = BlockFormattingContext::new();
                let mut root_ctx = root_bfc.root_block_context();
                self.compute_inline_layout_inner(
                    node_id,
                    LayoutInput {
                        known_dimensions: styled_based_known_dimensions,
                        ..inputs
                    },
                    &mut root_ctx,
                )
            }
        }
    }

    fn compute_inline_layout_inner(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
        _block_ctx: &mut BlockContext<'_>,
    ) -> taffy::LayoutOutput {
        let box_id = BoxId::from(node_id);
        let LayoutInput {
            known_dimensions,
            parent_size,
            available_space,
            sizing_mode,
            ..
        } = inputs;

        // Take the inline layout out of the box to satisfy the borrow checker
        let mut inline_layout = self
            .box_mut(box_id)
            .ifc
            .take()
            .expect("inline root without IFC data");

        let this = self.box_(box_id);
        let style = &this.style;

        // Note: both horizontal and vertical percentage padding/borders are
        // resolved against the container's inline size (i.e. width). This is
        // not a bug, but is how CSS is specified (see:
        // https://developer.mozilla.org/en-US/docs/Web/CSS/padding#values)
        let margin = style
            .margin()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let padding = style
            .padding()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let border = style
            .border()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let container_pb = padding + border;
        let pb_sum = container_pb.sum_axes();
        let box_sizing_adjustment = if style.box_sizing() == BoxSizing::ContentBox {
            pb_sum
        } else {
            Size::ZERO
        };

        // Scrollbar gutters are reserved when the `overflow` property is set
        // to `Overflow::Scroll`. However, the axes are switched (transposed)
        // because a node that scrolls vertically needs *horizontal* space to
        // be reserved for a scrollbar
        let scrollbar_gutter = style.overflow().transpose().map(|overflow| match overflow {
            Overflow::Scroll => style.scrollbar_width(),
            _ => 0.0,
        });
        // TODO: make side configurable based on the `direction` property
        let mut content_box_inset = container_pb;
        content_box_inset.right += scrollbar_gutter.x;
        content_box_inset.bottom += scrollbar_gutter.y;

        let is_scroll_container =
            style.overflow().x.is_scroll_container() || style.overflow().y.is_scroll_container();
        let has_styles_preventing_being_collapsed_through = !style.is_block()
            || is_scroll_container
            || style.position() == Position::Absolute
            || padding.top > 0.0
            || padding.bottom > 0.0
            || border.top > 0.0
            || border.bottom > 0.0;

        let text_align = this.text_align;
        let text_indent = this.text_indent.clone();

        // Short circuit if inline context contains no text or inline boxes
        if !has_styles_preventing_being_collapsed_through
            && inline_layout.text.is_empty()
            && inline_layout.layout.inline_boxes().is_empty()
        {
            // Put layout back
            self.box_mut(box_id).ifc = Some(inline_layout);
            return LayoutOutput::from_outer_size(
                Size::ZERO.maybe_max(container_pb.sum_axes().map(Some)),
            );
        }

        let (node_size, node_min_size, node_max_size, aspect_ratio) = resolve_node_sizes(
            &self.box_(box_id).style,
            known_dimensions,
            parent_size,
            sizing_mode,
            box_sizing_adjustment,
        );

        // Compute available space
        let available_space = Size {
            width: known_dimensions
                .width
                .map(AvailableSpace::from)
                .unwrap_or(available_space.width)
                .maybe_sub(margin.horizontal_axis_sum())
                .maybe_set(known_dimensions.width)
                .maybe_set(node_size.width)
                .map_definite_value(|size| {
                    size.maybe_clamp(node_min_size.width, node_max_size.width)
                        - content_box_inset.horizontal_axis_sum()
                }),
            height: known_dimensions
                .height
                .map(AvailableSpace::from)
                .unwrap_or(available_space.height)
                .maybe_sub(margin.vertical_axis_sum())
                .maybe_set(known_dimensions.height)
                .maybe_set(node_size.height)
                .map_definite_value(|size| {
                    size.maybe_clamp(node_min_size.height, node_max_size.height)
                        - content_box_inset.vertical_axis_sum()
                }),
        };

        // Compute size of inline boxes
        let child_inputs = taffy::tree::LayoutInput {
            known_dimensions: Size::NONE,
            available_space,
            sizing_mode: SizingMode::InherentSize,
            parent_size: available_space.into_options(),
            ..inputs
        };

        // Update inline boxes
        for ibox in inline_layout.layout.inline_boxes_mut() {
            let child_box_id = BoxId::from(taffy::NodeId::from(ibox.id));
            let style = &self.box_(child_box_id).style;
            let margin = style
                .margin
                .resolve_or_zero(inputs.parent_size, resolve_calc_value);

            if style.position == Position::Absolute {
                ibox.width = 0.0;
                ibox.height = 0.0;
            } else {
                let output = self.compute_child_layout(taffy::NodeId::from(ibox.id), child_inputs);
                ibox.width = margin.left + margin.right + output.size.width;
                ibox.height = margin.top + margin.bottom + output.size.height;
            }
        }

        // TODO: Resolve against style widths as well as known dimensions
        let resolved_text_indent = text_indent
            .length
            .resolve(CSSPixelLength::new(known_dimensions.width.unwrap_or(0.0)))
            .px();
        inline_layout.layout.set_text_indent(
            resolved_text_indent,
            // NOTE: hanging and each_line don't currently work because
            // parsing them is cfg'd out in stylo (Servo doesn't support them
            // yet).
            IndentOptions {
                each_line: text_indent.each_line,
                hanging: text_indent.hanging,
            },
        );

        let pbw = container_pb.horizontal_components().sum();
        let width = known_dimensions.width.map(|w| w - pbw).unwrap_or_else(|| {
            // TODO: Cache content widths.
            let content_sizes = inline_layout.layout.calculate_content_widths();
            let min_content_width = content_sizes.min;
            let max_content_width = content_sizes.max;

            let computed_width = match available_space.width {
                AvailableSpace::MinContent => min_content_width,
                AvailableSpace::MaxContent => max_content_width,
                AvailableSpace::Definite(limit) => {
                    limit.min(max_content_width).max(min_content_width)
                }
            }
            .ceil();

            let style_width = node_size.width;
            let min_width = node_min_size.width;
            let max_width = node_max_size.width;

            (style_width)
                .unwrap_or(computed_width + pbw)
                .max(computed_width)
                .maybe_clamp(min_width, max_width)
                - pbw
        });

        // Perform inline layout
        inline_layout.layout.break_all_lines(Some(width));

        inline_layout.layout.align(
            text_align,
            AlignmentOptions {
                align_when_overflowing: false,
            },
        );

        let height = inline_layout.layout.height();

        let final_size = inputs
            .known_dimensions
            .unwrap_or(taffy::Size { width, height });

        // Store sizes and positions of inline boxes
        for line in inline_layout.layout.lines() {
            for item in line.items() {
                if let parley::layout::PositionedLayoutItem::InlineBox(ibox) = item {
                    let child_box_id = BoxId::from(taffy::NodeId::from(ibox.id));
                    let child = self.box_(child_box_id);
                    let padding = child
                        .style
                        .padding
                        .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);
                    let border = child
                        .style
                        .border
                        .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);
                    let margin = child
                        .style
                        .margin
                        .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);

                    // Resolve inset
                    let left = child
                        .style
                        .inset
                        .left
                        .maybe_resolve(final_size.width, resolve_calc_value);
                    let right = child
                        .style
                        .inset
                        .right
                        .maybe_resolve(final_size.width, resolve_calc_value);
                    let top = child
                        .style
                        .inset
                        .top
                        .maybe_resolve(final_size.height, resolve_calc_value);
                    let bottom = child
                        .style
                        .inset
                        .bottom
                        .maybe_resolve(final_size.height, resolve_calc_value);

                    if child.style.position == Position::Absolute {
                        let output =
                            self.compute_child_layout(taffy::NodeId::from(ibox.id), child_inputs);

                        let layout = &mut self.box_mut(child_box_id).unrounded_layout;
                        layout.size = output.size;

                        // TODO: Implement absolute positioning against the
                        // nearest positioned ancestor (ADR-0006 §5: v1
                        // positions against the direct parent box).
                        layout.location.x = left
                            .map(|left| left + margin.left)
                            .or_else(|| {
                                right.map(|right| {
                                    final_size.width - right - output.size.width - margin.right
                                })
                            })
                            .unwrap_or(ibox.x + margin.left + container_pb.left);
                        layout.location.y = top
                            .map(|top| top + margin.top)
                            .or_else(|| {
                                bottom.map(|bottom| {
                                    final_size.height - bottom - output.size.height - margin.bottom
                                })
                            })
                            .unwrap_or(ibox.y + margin.top + container_pb.top);

                        layout.padding = padding;
                        layout.border = border;
                    } else {
                        let layout = &mut self.box_mut(child_box_id).unrounded_layout;
                        layout.size.width = ibox.width - margin.left - margin.right;
                        layout.size.height = ibox.height - margin.top - margin.bottom;
                        layout.location.x = ibox.x + margin.left + container_pb.left;
                        layout.location.y = ibox.y + margin.top + container_pb.top;
                        layout.padding = padding;
                        layout.border = border;
                    }
                }
            }
        }

        // The node's first baseline, in its own border-box-relative
        // coordinates (taffy's contract): the first IFC line's own baseline,
        // offset by the content box's inset from the border box. Read before
        // `inline_layout` moves back into the box below.
        //
        // Suppressed for a scroll container (CSS 2.1 §10.8.1's `inline-block`
        // baseline rule, which flex/grid's synthesized-baseline fallback
        // shares): once `overflow` computes to anything but `visible`, the
        // baseline is the bottom margin edge, not the content's first line —
        // `None` here makes `taffy`'s `unwrap_or(size.height)` fallback do
        // exactly that. `is_scroll_container` computed above, alongside
        // `has_styles_preventing_being_collapsed_through`.
        let first_baseline_y = if is_scroll_container {
            None
        } else {
            inline_layout
                .layout
                .lines()
                .next()
                .map(|line| content_box_inset.top + line.metrics().baseline)
        };

        // Put layout back
        self.box_mut(box_id).ifc = Some(inline_layout);

        let measured_size = final_size;

        let clamped_size = inputs
            .known_dimensions
            .or(node_size)
            .unwrap_or(measured_size + content_box_inset.sum_axes())
            .maybe_clamp(node_min_size, node_max_size);
        let size = Size {
            width: clamped_size.width,
            height: clamped_size.height.max(
                aspect_ratio
                    .map(|ratio| clamped_size.width / ratio)
                    .unwrap_or(0.0),
            ),
        };
        let size = size.maybe_max(container_pb.sum_axes().map(Some));

        LayoutOutput {
            size,
            content_size: measured_size + padding.sum_axes(),
            first_baselines: Point {
                x: None,
                y: first_baseline_y,
            },
            top_margin: CollapsibleMarginSet::ZERO,
            bottom_margin: CollapsibleMarginSet::ZERO,
            margins_can_collapse_through: !has_styles_preventing_being_collapsed_through
                && size.height == 0.0
                && measured_size.height == 0.0,
        }
    }
}
