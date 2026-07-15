//! Taffy tree traits on [`LayoutTree`] plus the compute dispatcher (adapted
//! from blitz-dom `layout/mod.rs`).
//!
//! This module (and its siblings `inline`/`replaced`/`overflow`) never
//! touches the DOM or stylo styles: everything was captured on the boxes at
//! construction time.

use style::Atom;
use style::values::computed::CSSPixelLength;
use style::values::computed::length_percentage::CalcLengthPercentage;
use taffy::{
    AvailableSpace, BlockContext, BoxGenerationMode, BoxSizing, Clear, CollapsibleMarginSet,
    CoreStyle, Dimension, Direction, Float, Layout, LayoutPartialTree, LengthPercentage,
    LengthPercentageAuto, MaybeResolve as _, NodeId, Overflow, Point, Position, Rect,
    ResolveOrZero as _, RunMode, Size, SizingMode, Style, TraversePartialTree, TraverseTree,
    compute_block_layout, compute_cached_layout, compute_flexbox_layout, compute_grid_layout,
    compute_leaf_layout,
};

use crate::replaced::replaced_measure_function;
use crate::tree::{BoxId, BoxKind, LayoutTree, ReplacedContent};

/// Resolves a taffy `calc()` handle back to the stylo expression it wraps.
///
/// SAFETY: the only `calc` values in our taffy styles are created by
/// `stylo_taffy::to_taffy_style`, which stores a pointer to a
/// `CalcLengthPercentage` owned by the `ComputedValues` captured on the
/// originating box; those `ComputedValues` outlive the layout pass.
#[allow(unsafe_code)]
pub(crate) fn resolve_calc_value(calc_ptr: *const (), parent_size: f32) -> f32 {
    let calc = unsafe { &*(calc_ptr as *const CalcLengthPercentage) };
    calc.resolve(CSSPixelLength::new(parent_size)).px()
}

impl LayoutTree {
    fn layout_box_from_taffy(&self, node_id: NodeId) -> &crate::tree::LayoutBox {
        self.box_(BoxId::from(node_id))
    }

    fn layout_box_from_taffy_mut(&mut self, node_id: NodeId) -> &mut crate::tree::LayoutBox {
        self.box_mut(BoxId::from(node_id))
    }

    /// Removes corrections from the previous pass before taffy consults its
    /// layout cache. Cached children otherwise accumulate the same relative
    /// inset on every incremental reflow.
    pub(crate) fn reset_post_layout_offsets(&mut self) {
        for layout_box in &mut self.boxes {
            layout_box.unrounded_layout.location.x -= layout_box.post_layout_offset.x;
            layout_box.unrounded_layout.location.y -= layout_box.post_layout_offset.y;
            layout_box.post_layout_offset = Point::ZERO;
        }
    }

    /// Applies CSS relative-position insets that taffy's float branch omits.
    /// This happens after normal-flow layout because relative positioning is a
    /// visual offset and must not influence float placement or parent height.
    pub(crate) fn apply_relative_float_offsets(&mut self) {
        let mut offsets = Vec::with_capacity(self.boxes.len());

        for layout_box in &self.boxes {
            if layout_box.style.float == Float::None
                || layout_box.position != style::computed_values::position::T::Relative
            {
                offsets.push(Point::ZERO);
                continue;
            }

            let (containing_size, direction) = layout_box.parent.map_or_else(
                || {
                    (
                        Size {
                            width: self.viewport.width,
                            height: self.viewport.height,
                        },
                        Direction::Ltr,
                    )
                },
                |parent| {
                    let parent = self.box_(parent);
                    let layout = parent.unrounded_layout;
                    (
                        Size {
                            width: (layout.size.width
                                - layout.padding.left
                                - layout.padding.right
                                - layout.border.left
                                - layout.border.right
                                - layout.scrollbar_size.width)
                                .max(0.0),
                            height: (layout.size.height
                                - layout.padding.top
                                - layout.padding.bottom
                                - layout.border.top
                                - layout.border.bottom
                                - layout.scrollbar_size.height)
                                .max(0.0),
                        },
                        CoreStyle::direction(&parent.style),
                    )
                },
            );
            let inset = layout_box.style.inset;
            let left = inset
                .left
                .maybe_resolve(containing_size.width, resolve_calc_value);
            let right = inset
                .right
                .maybe_resolve(containing_size.width, resolve_calc_value);
            let top = inset
                .top
                .maybe_resolve(containing_size.height, resolve_calc_value);
            let bottom = inset
                .bottom
                .maybe_resolve(containing_size.height, resolve_calc_value);

            offsets.push(Point {
                x: if direction == Direction::Rtl {
                    right.map(|value| -value).or(left).unwrap_or(0.0)
                } else {
                    left.or(right.map(|value| -value)).unwrap_or(0.0)
                },
                y: top.or(bottom.map(|value| -value)).unwrap_or(0.0),
            });
        }

        for (layout_box, offset) in self.boxes.iter_mut().zip(offsets) {
            layout_box.unrounded_layout.location.x += offset.x;
            layout_box.unrounded_layout.location.y += offset.y;
            layout_box.post_layout_offset = offset;
        }
    }

    /// Recomputes auto margins for absolutely positioned boxes when both
    /// opposing insets and the axis size are definite. Taffy currently tests
    /// the element size against the *remaining* free space, which incorrectly
    /// zeroes both horizontal margins for common centered sidebars such as
    /// `left: 0; right: 0; width: 155px; margin: auto`.
    pub(crate) fn apply_absolute_auto_margins(&mut self) {
        let mut corrections = Vec::with_capacity(self.boxes.len());

        for layout_box in &self.boxes {
            let Some(parent_id) = layout_box.parent else {
                corrections.push(None);
                continue;
            };
            if layout_box.style.position != Position::Absolute {
                corrections.push(None);
                continue;
            }

            let parent = self.box_(parent_id);
            let parent_layout = parent.unrounded_layout;
            let area_size = Size {
                width: (parent_layout.size.width
                    - parent_layout.border.left
                    - parent_layout.border.right
                    - parent_layout.scrollbar_size.width)
                    .max(0.0),
                height: (parent_layout.size.height
                    - parent_layout.border.top
                    - parent_layout.border.bottom
                    - parent_layout.scrollbar_size.height)
                    .max(0.0),
            };
            let area_offset = Point {
                x: parent_layout.border.left,
                y: parent_layout.border.top,
            };
            let inset = layout_box.style.inset;
            let left = inset
                .left
                .maybe_resolve(area_size.width, resolve_calc_value);
            let right = inset
                .right
                .maybe_resolve(area_size.width, resolve_calc_value);
            let top = inset
                .top
                .maybe_resolve(area_size.height, resolve_calc_value);
            let bottom = inset
                .bottom
                .maybe_resolve(area_size.height, resolve_calc_value);
            let raw_margin = layout_box.style.margin;
            let size = layout_box.unrounded_layout.size;
            let mut resolved_margin = layout_box.unrounded_layout.margin;
            let mut target = layout_box.unrounded_layout.location;
            let mut changed = false;
            let rtl = CoreStyle::direction(&parent.style) == Direction::Rtl;

            // Percentage margins resolve against the containing-block *width* on
            // both axes (CSS), so `area_size.width` is the percentage basis for
            // each. Only the horizontal axis reverses under RTL.
            if let Some((margin_left, margin_right, tx)) = resolve_absolute_auto_margin_axis(
                layout_box.style.size.width.is_auto(),
                left,
                right,
                raw_margin.left,
                raw_margin.right,
                size.width,
                area_size.width,
                area_offset.x,
                area_size.width,
                rtl,
            ) {
                resolved_margin.left = margin_left;
                resolved_margin.right = margin_right;
                target.x = tx;
                changed = true;
            }

            if let Some((margin_top, margin_bottom, ty)) = resolve_absolute_auto_margin_axis(
                layout_box.style.size.height.is_auto(),
                top,
                bottom,
                raw_margin.top,
                raw_margin.bottom,
                size.height,
                area_size.height,
                area_offset.y,
                area_size.width,
                false,
            ) {
                resolved_margin.top = margin_top;
                resolved_margin.bottom = margin_bottom;
                target.y = ty;
                changed = true;
            }

            corrections.push(changed.then_some((target, resolved_margin)));
        }

        for (layout_box, correction) in self.boxes.iter_mut().zip(corrections) {
            let Some((target, resolved_margin)) = correction else {
                continue;
            };
            let offset = Point {
                x: target.x - layout_box.unrounded_layout.location.x,
                y: target.y - layout_box.unrounded_layout.location.y,
            };
            layout_box.unrounded_layout.location = target;
            layout_box.unrounded_layout.margin = resolved_margin;
            layout_box.post_layout_offset.x += offset.x;
            layout_box.post_layout_offset.y += offset.y;
        }
    }

    /// Caps each flex item's intrinsic main-size contribution at its flex base
    /// size, by zeroing `flex-grow` for the duration of one `compute_flexbox_layout`
    /// call. Returns the items it touched, for the caller to restore.
    ///
    /// CSS Flexbox §9.9.1 says an item's max-content contribution is clamped by
    /// its flex base size only when the item cannot grow, and taffy implements
    /// exactly that (`flex_basis_max` is gated on `flex_grow == 0.0`,
    /// flexbox.rs:1023). Gecko, Blink and WebKit all clamp by the flex base size
    /// regardless of the grow factor, and `flex-one-sets-flex-basis-to-zero-px.html`
    /// tests for the shipped behaviour, not the spec's: `flex: 1 1 0px` and
    /// `flex: 0.5 1 0px` in an auto-height column flex container both collapse to
    /// 0, where the spec would give the item's 14px max-content size (and, for the
    /// 0.5 factor, an interpolated 7px). The test says so in as many words.
    ///
    /// Zeroing the grow factor is sound *only* under the conditions checked here.
    /// The latch is live for the whole call, so it also reaches
    /// `resolve_flexible_lengths` — but with the container's main size gone to the
    /// sum of the flex base sizes there is no free space left to distribute, so
    /// growing was already a no-op. That equivalence breaks the moment the
    /// container's own `min`/`max` main size can clamp that sum into a different
    /// number: a `min-height` container *does* open up free space its items must
    /// grow into, and a zeroed factor would strand them at their base size. Hence
    /// the guard, and hence this must never fire when taffy is being handed a main
    /// size rather than deriving one.
    ///
    /// Items whose flex basis is `auto`, `content` or an unresolved percentage are
    /// untouched: taffy derives their base size from content, so capping at it is
    /// what it already does.
    fn hide_flex_grow_for_intrinsic_main_size(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
    ) -> Vec<(BoxId, f32)> {
        let children = {
            let this = self.layout_box_from_taffy(node_id);
            let style = &this.style;
            let is_row = matches!(
                style.flex_direction,
                taffy::FlexDirection::Row | taffy::FlexDirection::RowReverse
            );
            let (main_known, main_available, main_size, main_min, main_max) = if is_row {
                (
                    inputs.known_dimensions.width,
                    inputs.available_space.width,
                    style.size.width,
                    style.min_size.width,
                    style.max_size.width,
                )
            } else {
                (
                    inputs.known_dimensions.height,
                    inputs.available_space.height,
                    style.size.height,
                    style.min_size.height,
                    style.max_size.height,
                )
            };
            // `known_dimensions` alone is not the test: `compute_flexbox_layout`
            // *derives* a known main size from the container's own `size` style
            // when the caller left it open (`clamped_style_size`, flexbox.rs:192),
            // and a container sized that way distributes free space exactly like
            // one handed a size. Missing this let the latch fire on
            // `image-as-flexitem-size-006.html`'s `height: 40px` column, where it
            // stranded a `flex: 1 1 30px` image at 30px instead of letting it grow.
            let deriving_main_size = main_known.is_none()
                && main_size.is_auto()
                && matches!(
                    main_available,
                    AvailableSpace::MinContent | AvailableSpace::MaxContent
                );
            if !deriving_main_size || !main_min.is_auto() || !main_max.is_auto() {
                return Vec::new();
            }
            this.children.clone()
        };

        let mut hidden = Vec::new();
        for child in children {
            let style = &mut self.box_mut(child).style;
            let basis_is_a_length =
                style.flex_basis.into_raw().tag() == taffy::CompactLength::LENGTH_TAG;
            if style.flex_grow == 0.0 || !basis_is_a_length {
                continue;
            }
            hidden.push((child, std::mem::replace(&mut style.flex_grow, 0.0)));
        }
        hidden
    }

    /// The compute dispatcher (blitz-dom `compute_child_layout_internal`).
    fn compute_box_layout(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> taffy::LayoutOutput {
        let layout_box = self.layout_box_from_taffy(node_id);
        let kind = layout_box.kind;
        let has_ifc = layout_box.ifc.is_some();
        let font_size = layout_box.font_size;
        let line_height = layout_box.line_height;
        let replaced = layout_box.replaced.clone();
        let force_bfc = layout_box.force_bfc;

        match kind {
            BoxKind::Replaced => match replaced.expect("Replaced box without replaced content") {
                ReplacedContent::Image(context) => {
                    // A block-level replaced element with `width: auto` uses its
                    // intrinsic width (CSS 2.2 §10.3.4), not the container width
                    // taffy's block algorithm stretches a block child to. Drop
                    // the stretched known width so the replaced measure resolves
                    // the intrinsic (or aspect-derived) size. Flex/grid items
                    // keep the size their container resolved — they may
                    // legitimately grow/shrink.
                    let (parent, width_is_auto) = {
                        let this = self.layout_box_from_taffy(node_id);
                        (this.parent, this.style.size.width.is_auto())
                    };
                    let parent_is_block =
                        parent.is_some_and(|p| self.box_(p).style.display == taffy::Display::Block);
                    let mut known = inputs.known_dimensions;
                    if width_is_auto && parent_is_block {
                        known.width = None;
                    }

                    let style = &self.layout_box_from_taffy(node_id).style;
                    let computed = replaced_measure_function(
                        known,
                        inputs.parent_size,
                        inputs.available_space,
                        &context,
                        style,
                    );
                    taffy::LayoutOutput {
                        size: computed,
                        content_size: computed,
                        first_baselines: taffy::Point::NONE,
                        top_margin: CollapsibleMarginSet::ZERO,
                        bottom_margin: CollapsibleMarginSet::ZERO,
                        margins_can_collapse_through: false,
                    }
                }
                ReplacedContent::TextInput {
                    rows,
                    cols,
                    multiline,
                } => {
                    let style = &self.layout_box_from_taffy(node_id).style;
                    // CSS Sizing 3 §replaced-percentage-min-contribution: a
                    // replaced element's min-content contribution resolves a
                    // percentage in that axis against *zero*, not against the
                    // containing block. So `width: 100%` contributes 0 and
                    // `calc(140px + 100%)` contributes 140px. `replaced.rs`
                    // already does this for images (`basis_for_max_and_preferred`),
                    // but the form-control leaves bypass `replaced_measure_function`
                    // and never read `style.size` at all — they reported a bare
                    // intrinsic guess, and taffy's automatic-minimum-size clamp
                    // (`min(min_content, specified)`, flexbox.rs:838) then floored
                    // the item at the wrong value. `auto` keeps the intrinsic
                    // guess: the rule only speaks about a *specified* size.
                    let specified_min_content_width = (!style.size.width.is_auto()).then(|| {
                        style
                            .size
                            .width
                            .resolve_or_zero(Some(0.0), resolve_calc_value)
                    });
                    compute_leaf_layout(
                        inputs,
                        style,
                        resolve_calc_value,
                        |_known_size, available_space| {
                            let intrinsic_width = if multiline {
                                cols.map(|cols| cols * font_size * 0.6).unwrap_or(300.0)
                            } else {
                                match available_space.width {
                                    AvailableSpace::Definite(limit) => limit.min(300.0),
                                    AvailableSpace::MinContent => 0.0,
                                    AvailableSpace::MaxContent => 300.0,
                                }
                            };
                            taffy::Size {
                                width: if available_space.width == AvailableSpace::MinContent {
                                    specified_min_content_width.unwrap_or(intrinsic_width)
                                } else {
                                    intrinsic_width
                                },
                                height: line_height * rows,
                            }
                        },
                    )
                }
                ReplacedContent::Checkbox => {
                    let style = &self.layout_box_from_taffy(node_id).style;
                    compute_leaf_layout(
                        inputs,
                        style,
                        resolve_calc_value,
                        |_known_size, available_space| {
                            // Same §replaced-percentage-min-contribution rule as
                            // the text-control arm above.
                            let width_basis = if available_space.width == AvailableSpace::MinContent
                            {
                                Some(0.0)
                            } else {
                                inputs.parent_size.width
                            };
                            let width = style
                                .size
                                .width
                                .resolve_or_zero(width_basis, resolve_calc_value);
                            let height = style
                                .size
                                .height
                                .resolve_or_zero(inputs.parent_size.height, resolve_calc_value);
                            let min_size = width.min(height);
                            taffy::Size {
                                width: min_size,
                                height: min_size,
                            }
                        },
                    )
                }
            },

            BoxKind::TableRoot => {
                let ctx = self
                    .layout_box_from_taffy(node_id)
                    .table
                    .clone()
                    .expect("TableRoot without table context");
                let mut wrapper = crate::table::TableTreeWrapper { tree: self, ctx };
                let mut output = compute_grid_layout(&mut wrapper, node_id, inputs);

                // Cap content size at node size to prevent scrolling
                // (blitz-dom's table hack).
                output.content_size.width = output.content_size.width.min(output.size.width);
                output.content_size.height = output.content_size.height.min(output.size.height);

                output
            }

            // A multicol container establishes its own formatting context, so it
            // does not join the parent's float/margin-collapse context.
            BoxKind::MulticolRoot => self.compute_multicol_layout(node_id, inputs),

            BoxKind::InlineRoot | BoxKind::AnonymousBlock if has_ifc => {
                self.compute_inline_layout(node_id, inputs, block_ctx)
            }

            _ => match self.layout_box_from_taffy(node_id).style.display {
                taffy::Display::Block => {
                    // Taffy folds `min-height` into the "style-based known
                    // size" a block hands down as the percentage basis for its
                    // children. CSS does not: a percentage height resolves
                    // against the containing block's *computed height*, and an
                    // `auto` height stays indefinite (the percentage then
                    // behaves as `auto`) no matter what `min-height` says.
                    // Leaving it in makes a `height: 100%` child collapse onto
                    // the minimum — on mgid.com `main` is `flex: 1 1 0%;
                    // min-height: 480px` wrapping a `height: 100%` element, so
                    // the whole page collapsed to 480px and the footer landed
                    // under the hero. Hide it from the inner pass and apply it
                    // as what it is: a lower bound on the result.
                    let hidden_min = {
                        let this = self.layout_box_from_taffy_mut(node_id);
                        (this.style.size.height.is_auto() && !this.style.min_size.height.is_auto())
                            .then(|| {
                                std::mem::replace(
                                    &mut this.style.min_size.height,
                                    Dimension::auto(),
                                )
                            })
                    };

                    // Under `SizingMode::ContentSize` the same reasoning reaches
                    // `size.height` itself. Taffy is being asked for a purely
                    // content-based measurement and already ignores the node's own
                    // size style when sizing the node — but it still folds
                    // `size.height` into `container_percentage_resolution_height`
                    // (taffy block.rs:461), the basis it hands children for
                    // percentage heights. CSS Flexbox §4.5 is explicit that a flex
                    // item's own height must not influence its content size
                    // suggestion, so a `height: 100%` child would otherwise resolve
                    // against the item's specified height and inflate the automatic
                    // minimum size straight back up to it.
                    //
                    // This latch is deliberately *independent* of `hidden_min`
                    // above, which is computed first, from the real `size.height`:
                    // folding the two together would make a `height: 10px;
                    // min-height: 50%` item take `hidden_min`'s restore path, whose
                    // floor resolves that percentage against the parent — the very
                    // thing CSS forbids when the flex container's height is
                    // indefinite (`percentage-size.html`).
                    let hidden_height =
                        (inputs.sizing_mode == SizingMode::ContentSize).then(|| {
                            let this = self.layout_box_from_taffy_mut(node_id);
                            std::mem::replace(&mut this.style.size.height, Dimension::auto())
                        });

                    let mut output = compute_block_layout(self, node_id, inputs, block_ctx);
                    if let Some(height) = hidden_height {
                        self.layout_box_from_taffy_mut(node_id).style.size.height = height;
                    }
                    if let Some(min_height) = hidden_min {
                        self.layout_box_from_taffy_mut(node_id)
                            .style
                            .min_size
                            .height = min_height;
                        if let Some(mut min) =
                            min_height.maybe_resolve(inputs.parent_size.height, resolve_calc_value)
                        {
                            // `output.size.height` is a border-box height. With
                            // the default `content-box` sizing, `min-height`
                            // bounds the *content* box, so lift it to a
                            // border-box floor by adding the block's vertical
                            // padding + border (`border-box` sizing already
                            // includes them). Percentage padding/border resolve
                            // against the containing-block inline size.
                            let style = &self.layout_box_from_taffy(node_id).style;
                            if style.box_sizing == BoxSizing::ContentBox {
                                let padding = style
                                    .padding
                                    .resolve_or_zero(inputs.parent_size.width, resolve_calc_value);
                                let border = style
                                    .border
                                    .resolve_or_zero(inputs.parent_size.width, resolve_calc_value);
                                min += padding.top + padding.bottom + border.top + border.bottom;
                            }
                            output.size.height = output.size.height.max(min);
                        }
                    }

                    // Taffy's experimental float layout contains floats in a
                    // BFC using max(in-flow-height, tallest-float-bottom). If
                    // the final float is shorter, a following clear box may
                    // end before the tallest float, and bottom padding is then
                    // lost by that max(). An auto-height BFC must contain the
                    // entire float margin box *and* its own bottom inset.
                    if force_bfc
                        && inputs.run_mode == RunMode::PerformLayout
                        && self
                            .layout_box_from_taffy(node_id)
                            .style
                            .size
                            .height
                            .is_auto()
                    {
                        let children = self.layout_box_from_taffy(node_id).children.clone();
                        let float_bottom = children
                            .into_iter()
                            .filter_map(|child_id| {
                                let child = self.box_(child_id);
                                (child.style.float != Float::None
                                    && child.style.position != Position::Absolute)
                                    .then_some(
                                        child.unrounded_layout.location.y
                                            + child.unrounded_layout.size.height
                                            + child.unrounded_layout.margin.bottom,
                                    )
                            })
                            .fold(0.0_f32, f32::max);

                        if float_bottom > 0.0 {
                            let style = &self.layout_box_from_taffy(node_id).style;
                            let padding = style
                                .padding
                                .resolve_or_zero(Some(output.size.width), resolve_calc_value);
                            let border = style
                                .border
                                .resolve_or_zero(Some(output.size.width), resolve_calc_value);
                            output.size.height = output
                                .size
                                .height
                                .max(float_bottom + padding.bottom + border.bottom);
                        }
                    }

                    output
                }
                taffy::Display::Flex => {
                    let hidden_grow = self.hide_flex_grow_for_intrinsic_main_size(node_id, inputs);
                    let output = compute_flexbox_layout(self, node_id, inputs);
                    for (child, flex_grow) in hidden_grow {
                        self.box_mut(child).style.flex_grow = flex_grow;
                    }
                    output
                }
                taffy::Display::Grid => compute_grid_layout(self, node_id, inputs),
                taffy::Display::None => taffy::LayoutOutput::HIDDEN,
            },
        }
    }
}

/// Resolves auto margins and the placement offset for one axis of an
/// absolutely positioned box whose opposing insets and size are both definite
/// (the shared body of the horizontal and vertical passes in
/// [`LayoutTree::apply_absolute_auto_margins`]). Returns
/// `(margin_start, margin_end, target)` when a correction applies, else `None`.
///
/// `percent_basis` is the containing-block width against which percentage
/// margins resolve on *both* axes; `rtl` reverses the free space and anchors
/// placement to the end edge (horizontal axis only — callers pass `false` for
/// the vertical one).
#[allow(clippy::too_many_arguments)]
fn resolve_absolute_auto_margin_axis(
    size_is_auto: bool,
    start_inset: Option<f32>,
    end_inset: Option<f32>,
    raw_start: LengthPercentageAuto,
    raw_end: LengthPercentageAuto,
    size: f32,
    area_size: f32,
    area_offset: f32,
    percent_basis: f32,
    rtl: bool,
) -> Option<(f32, f32, f32)> {
    if size_is_auto {
        return None;
    }
    let (Some(start), Some(end)) = (start_inset, end_inset) else {
        return None;
    };
    if !(raw_start.is_auto() || raw_end.is_auto()) {
        return None;
    }
    let fixed_start = raw_start
        .maybe_resolve(percent_basis, resolve_calc_value)
        .unwrap_or(0.0);
    let fixed_end = raw_end
        .maybe_resolve(percent_basis, resolve_calc_value)
        .unwrap_or(0.0);
    let free = area_size - start - end - size - fixed_start - fixed_end;
    let (margin_start, margin_end) = resolve_absolute_auto_margin_pair(
        raw_start.is_auto(),
        raw_end.is_auto(),
        fixed_start,
        fixed_end,
        free,
        rtl,
    );
    let target = if rtl {
        area_offset + area_size - size - end - margin_end
    } else {
        area_offset + start + margin_start
    };
    Some((margin_start, margin_end, target))
}

fn resolve_absolute_auto_margin_pair(
    start_auto: bool,
    end_auto: bool,
    fixed_start: f32,
    fixed_end: f32,
    free: f32,
    reverse_negative: bool,
) -> (f32, f32) {
    match (start_auto, end_auto) {
        (true, true) if free >= 0.0 => (free / 2.0, free / 2.0),
        (true, true) if reverse_negative => (free, 0.0),
        (true, true) => (0.0, free),
        (true, false) => (free, fixed_end),
        (false, true) => (fixed_start, free),
        (false, false) => (fixed_start, fixed_end),
    }
}

/// Child iterator over a box's `children` (cloned ids; boxes are stored in a
/// flat arena, so no borrow gymnastics are needed).
pub struct ChildIter {
    children: Vec<BoxId>,
    idx: usize,
}

impl Iterator for ChildIter {
    type Item = NodeId;
    fn next(&mut self) -> Option<Self::Item> {
        let id = self.children.get(self.idx)?;
        self.idx += 1;
        Some((*id).into())
    }
}

impl TraversePartialTree for LayoutTree {
    type ChildIter<'a> = ChildIter;

    fn child_ids(&self, node_id: NodeId) -> Self::ChildIter<'_> {
        ChildIter {
            children: self.layout_box_from_taffy(node_id).children.clone(),
            idx: 0,
        }
    }

    fn child_count(&self, node_id: NodeId) -> usize {
        self.layout_box_from_taffy(node_id).children.len()
    }

    fn get_child_id(&self, node_id: NodeId, index: usize) -> NodeId {
        self.layout_box_from_taffy(node_id).children[index].into()
    }
}

impl TraverseTree for LayoutTree {}

/// A borrowed block-item style that can make a clearfix container establish
/// a layout BFC without changing its computed overflow (which paint and
/// geometry must continue to observe).
pub struct BlockItemStyleRef<'a> {
    style: &'a Style<Atom>,
    force_bfc: bool,
}

impl CoreStyle for BlockItemStyleRef<'_> {
    type CustomIdent = Atom;

    fn box_generation_mode(&self) -> BoxGenerationMode {
        CoreStyle::box_generation_mode(self.style)
    }

    fn is_block(&self) -> bool {
        CoreStyle::is_block(self.style)
    }

    fn is_compressible_replaced(&self) -> bool {
        CoreStyle::is_compressible_replaced(self.style)
    }

    fn box_sizing(&self) -> BoxSizing {
        CoreStyle::box_sizing(self.style)
    }

    fn direction(&self) -> Direction {
        CoreStyle::direction(self.style)
    }

    fn overflow(&self) -> Point<Overflow> {
        if self.force_bfc {
            Point {
                x: Overflow::Hidden,
                y: Overflow::Hidden,
            }
        } else {
            CoreStyle::overflow(self.style)
        }
    }

    fn scrollbar_width(&self) -> f32 {
        CoreStyle::scrollbar_width(self.style)
    }

    fn position(&self) -> Position {
        CoreStyle::position(self.style)
    }

    fn inset(&self) -> Rect<LengthPercentageAuto> {
        CoreStyle::inset(self.style)
    }

    fn size(&self) -> Size<Dimension> {
        CoreStyle::size(self.style)
    }

    fn min_size(&self) -> Size<Dimension> {
        CoreStyle::min_size(self.style)
    }

    fn max_size(&self) -> Size<Dimension> {
        CoreStyle::max_size(self.style)
    }

    fn aspect_ratio(&self) -> Option<f32> {
        CoreStyle::aspect_ratio(self.style)
    }

    fn margin(&self) -> Rect<LengthPercentageAuto> {
        CoreStyle::margin(self.style)
    }

    fn padding(&self) -> Rect<LengthPercentage> {
        CoreStyle::padding(self.style)
    }

    fn border(&self) -> Rect<LengthPercentage> {
        CoreStyle::border(self.style)
    }
}

impl taffy::BlockItemStyle for BlockItemStyleRef<'_> {
    fn is_table(&self) -> bool {
        self.style.item_is_table
    }

    fn float(&self) -> Float {
        self.style.float
    }

    fn clear(&self) -> Clear {
        self.style.clear
    }
}

impl LayoutPartialTree for LayoutTree {
    type CoreContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type CustomIdent = Atom;

    fn get_core_container_style(&self, node_id: NodeId) -> &Style<Atom> {
        &self.layout_box_from_taffy(node_id).style
    }

    fn set_unrounded_layout(&mut self, node_id: NodeId, layout: &Layout) {
        self.layout_box_from_taffy_mut(node_id).unrounded_layout = *layout;
    }

    fn resolve_calc_value(&self, calc_ptr: *const (), parent_size: f32) -> f32 {
        resolve_calc_value(calc_ptr, parent_size)
    }

    #[inline(always)]
    fn compute_child_layout(
        &mut self,
        node_id: NodeId,
        inputs: taffy::LayoutInput,
    ) -> taffy::LayoutOutput {
        compute_cached_layout(self, node_id, inputs, |tree, node_id, inputs| {
            tree.compute_box_layout(node_id, inputs, None)
        })
    }
}

impl taffy::CacheTree for LayoutTree {
    #[inline]
    fn cache_get(
        &self,
        node_id: NodeId,
        inputs: &taffy::LayoutInput,
    ) -> Option<taffy::LayoutOutput> {
        self.layout_box_from_taffy(node_id).cache.get(inputs)
    }

    #[inline]
    fn cache_store(
        &mut self,
        node_id: NodeId,
        inputs: &taffy::LayoutInput,
        layout_output: taffy::LayoutOutput,
    ) {
        self.layout_box_from_taffy_mut(node_id)
            .cache
            .store(inputs, layout_output);
    }

    #[inline]
    fn cache_clear(&mut self, node_id: NodeId) {
        self.layout_box_from_taffy_mut(node_id).cache.clear();
    }
}

impl taffy::LayoutBlockContainer for LayoutTree {
    type BlockContainerStyle<'a>
        = &'a Style<Atom>
    where
        Self: 'a;

    type BlockItemStyle<'a>
        = BlockItemStyleRef<'a>
    where
        Self: 'a;

    fn get_block_container_style(&self, node_id: NodeId) -> Self::BlockContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_block_child_style(&self, child_node_id: NodeId) -> Self::BlockItemStyle<'_> {
        let child = self.layout_box_from_taffy(child_node_id);
        BlockItemStyleRef {
            style: &child.style,
            force_bfc: child.force_bfc,
        }
    }

    #[inline(always)]
    fn compute_block_child_layout(
        &mut self,
        node_id: NodeId,
        inputs: taffy::LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> taffy::LayoutOutput {
        compute_cached_layout(self, node_id, inputs, |tree, node_id, inputs| {
            tree.compute_box_layout(node_id, inputs, block_ctx)
        })
    }
}

impl taffy::LayoutFlexboxContainer for LayoutTree {
    type FlexboxContainerStyle<'a>
        = &'a Style<Atom>
    where
        Self: 'a;

    type FlexboxItemStyle<'a>
        = &'a Style<Atom>
    where
        Self: 'a;

    fn get_flexbox_container_style(&self, node_id: NodeId) -> Self::FlexboxContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_flexbox_child_style(&self, child_node_id: NodeId) -> Self::FlexboxItemStyle<'_> {
        self.get_core_container_style(child_node_id)
    }
}

impl taffy::LayoutGridContainer for LayoutTree {
    type GridContainerStyle<'a>
        = &'a Style<Atom>
    where
        Self: 'a;

    type GridItemStyle<'a>
        = &'a Style<Atom>
    where
        Self: 'a;

    fn get_grid_container_style(&self, node_id: NodeId) -> Self::GridContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_grid_child_style(&self, child_node_id: NodeId) -> Self::GridItemStyle<'_> {
        self.get_core_container_style(child_node_id)
    }
}

impl taffy::RoundTree for LayoutTree {
    fn get_unrounded_layout(&self, node_id: NodeId) -> Layout {
        self.layout_box_from_taffy(node_id).unrounded_layout
    }

    fn set_final_layout(&mut self, node_id: NodeId, layout: &Layout) {
        self.layout_box_from_taffy_mut(node_id).final_layout = *layout;
    }
}
