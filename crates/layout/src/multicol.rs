//! CSS multi-column layout (CSS Multicol Level 1) as **clipped, translated
//! views of one continuous flow** (ADR-0016).
//!
//! The box tree cannot represent one DOM node at two positions — `node_to_box`
//! is 1:1 and paint indexes exactly one origin per `BoxId` — so there is no
//! fragment tree here. Instead a [`BoxKind::MulticolRoot`] owns exactly one
//! child: an anonymous *flow* box holding all of the element's content, laid
//! out once as a single continuous block of the used column width. The compute
//! pass then slices that flow's block axis at real break opportunities (between
//! block boxes, between parley line boxes — never mid-line), paint shows each
//! slice through a clip + translate, and geometry maps flow coordinates into
//! the column that shows them.
//!
//! Correct pixels fall out for free: text flows across columns, and a block
//! straddling a boundary has its background sliced by the clip — which is
//! exactly `box-decoration-break: slice`, the CSS default.
//!
//! # Two coordinate spaces
//!
//! Boundaries are chosen twice, from the same structural walk:
//!
//! - during compute, from `unrounded_layout`, to pick *which* break
//!   opportunities the columns end at (and hence the container's height);
//! - after `taffy::round_layout`, from `final_layout`, to place them — because
//!   paint positions the flow's content from the **rounded** origins, and a
//!   boundary derived from the unrounded ones would sit up to half a pixel off,
//!   shaving the top line of every column but the first.
//!
//! [`break_opportunities`] therefore walks the subtree in a fixed structural
//! order and returns a list whose *length and ordering are identical* in both
//! spaces; [`MulticolContext::boundaries`] stores indices into it, and
//! [`resolve_columns`] turns those indices into pixel ranges once the layout is
//! rounded.

use style::properties::ComputedValues;
use style::values::computed::column::ColumnCount;
use style::values::computed::{CSSPixelLength, NonNegativeLengthPercentage};
use style::values::generics::length::{
    GenericLengthPercentageOrAuto, GenericLengthPercentageOrNormal,
};
use taffy::{
    AvailableSpace, BoxSizing, CollapsibleMarginSet, CoreStyle as _, LayoutInput, LayoutOutput,
    LayoutPartialTree as _, MaybeMath as _, Overflow, Point, RequestedAxis, ResolveOrZero as _,
    RunMode, Size, SizingMode,
};

use crate::taffy_impl::resolve_calc_value;
use crate::tree::{BoxId, BoxKind, LayoutBox, LayoutTree};

/// Hard cap on the used column count. `column-width: 0.01px` in a wide
/// container would otherwise ask for a hundred thousand clip + layer pairs (the
/// flow subtree is re-emitted once per column).
pub const MAX_COLUMNS: usize = 64;

/// Tolerance for comparing two break offsets, in CSS px. Well below a device
/// pixel, but far above the f32 noise a chain of additions accumulates.
const EPS: f32 = 0.01;

/// One used column: the slice `[start, end)` of the flow's block axis it shows,
/// and its inline offset — both in the multicol container's **content-box**
/// coordinate space.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ColumnRange {
    /// Inline offset of this column's left edge.
    pub x: f32,
    /// First flow-space `y` shown in this column.
    pub start: f32,
    /// One past the last flow-space `y` shown in this column. Columns are
    /// contiguous: `columns[k].end == columns[k + 1].start`.
    pub end: f32,
}

/// `column-gap`, kept unresolved: `normal` is 1em of the *container's* font and
/// a percentage resolves against its content inline size, neither of which is
/// known at construction time.
///
/// Read from stylo rather than from `taffy::Style::gap` on purpose: `stylo_taffy`
/// maps `normal` to `0px`, which is right for flex/grid and wrong here.
#[derive(Clone, Debug)]
pub(crate) enum ColumnGap {
    Normal,
    Length(NonNegativeLengthPercentage),
}

/// The `column-*` inputs captured at construction ([`LayoutBox`] does not retain
/// the stylo `ComputedValues`).
#[derive(Clone, Debug)]
pub(crate) struct MulticolConfig {
    /// `column-count` (`None` = `auto`).
    count: Option<u32>,
    /// `column-width` in CSS px (`None` = `auto`).
    width: Option<f32>,
    gap: ColumnGap,
}

impl MulticolConfig {
    fn from_style(style: &ComputedValues) -> Self {
        let column = style.get_column();
        let count = match column.column_count {
            ColumnCount::Integer(n) => u32::try_from(n.0).ok().filter(|&n| n > 0),
            ColumnCount::Auto => None,
        };
        let width = match &column.column_width {
            GenericLengthPercentageOrAuto::LengthPercentage(len) => Some(len.0.px()),
            GenericLengthPercentageOrAuto::Auto => None,
        };
        let gap = match &style.get_position().column_gap {
            GenericLengthPercentageOrNormal::LengthPercentage(len) => {
                ColumnGap::Length(len.clone())
            }
            GenericLengthPercentageOrNormal::Normal => ColumnGap::Normal,
        };
        Self { count, width, gap }
    }

    /// The used `column-gap`: `normal` is 1em (CSS Multicol §6.1), a percentage
    /// resolves against the container's content inline size.
    fn used_gap(&self, content_inline_size: f32, font_size: f32) -> f32 {
        match &self.gap {
            ColumnGap::Normal => font_size,
            ColumnGap::Length(len) => len
                .0
                .resolve(CSSPixelLength::new(content_inline_size))
                .px()
                .max(0.0),
        }
    }
}

/// Side context of a [`BoxKind::MulticolRoot`] (mirrors
/// [`crate::table::TableContext`]): the captured `column-*` inputs plus what the
/// compute pass derives from them.
#[derive(Debug)]
pub struct MulticolContext {
    config: MulticolConfig,
    /// The single anonymous flow box holding the element's content.
    flow: BoxId,

    // --- written by the compute pass ---
    /// Used column width (CSS px).
    used_width: f32,
    /// Used column gap (CSS px).
    used_gap: f32,
    /// Indices into [`break_opportunities`] at which the columns start and end;
    /// `len() == columns + 1`. Kept as indices, not pixels, so that
    /// [`resolve_columns`] can re-derive the pixel ranges in the *rounded*
    /// coordinate space paint works in (see the module docs).
    boundaries: Vec<usize>,

    // --- written by `resolve_columns`, read by paint / geometry / hit-test ---
    columns: Vec<ColumnRange>,
}

impl MulticolContext {
    /// The anonymous flow box holding this container's content.
    #[must_use]
    pub fn flow(&self) -> BoxId {
        self.flow
    }

    /// The used column width in CSS px.
    #[must_use]
    pub fn used_width(&self) -> f32 {
        self.used_width
    }

    /// The used `column-gap` in CSS px.
    #[must_use]
    pub fn used_gap(&self) -> f32 {
        self.used_gap
    }

    /// The used columns, in flow order.
    #[must_use]
    pub fn columns(&self) -> &[ColumnRange] {
        &self.columns
    }
}

/// Whether `style` makes a block container a multi-column container: CSS
/// Multicol §3 — neither `column-count` nor `column-width` is `auto`.
///
/// `column-rule-*` and `column-fill` are `engine = "gecko"` in this stylo build
/// and are not in the cascade at all, so they cannot participate (ADR-0016).
pub(crate) fn is_multicol(style: &ComputedValues) -> bool {
    let column = style.get_column();
    !column.column_count.is_auto()
        || !matches!(column.column_width, GenericLengthPercentageOrAuto::Auto)
}

/// Attaches a [`MulticolContext`] to `container`, whose content was built under
/// the anonymous `flow` box (see `construct::collect_multicol_children`).
pub(crate) fn make_multicol_root(
    tree: &mut LayoutTree,
    container: BoxId,
    flow: BoxId,
    style: &ComputedValues,
) {
    let b = tree.box_mut(container);
    b.kind = BoxKind::MulticolRoot;
    b.multicol = Some(Box::new(MulticolContext {
        config: MulticolConfig::from_style(style),
        flow,
        used_width: 0.0,
        used_gap: 0.0,
        boundaries: Vec::new(),
        columns: Vec::new(),
    }));
}

/// CSS Multicol §3.4: the used column count and width for a definite available
/// inline size. Construction gates on [`is_multicol`], so at least one of
/// `count`/`width` is definite here; `(None, None)` degenerates to one column.
fn used_columns(available: f32, count: Option<u32>, width: Option<f32>, gap: f32) -> (usize, f32) {
    /// `max(1, floor((available + gap) / (width + gap)))`, guarding the
    /// degenerate `width + gap == 0` (stylo permits `column-width: 0`).
    fn fit(available: f32, width: f32, gap: f32) -> usize {
        let denominator = width + gap;
        if denominator <= 0.0 {
            return MAX_COLUMNS;
        }
        let n = ((available + gap) / denominator).floor();
        if n.is_finite() && n >= 1.0 {
            (n as usize).min(MAX_COLUMNS)
        } else {
            1
        }
    }

    let available = available.max(0.0);
    let n = match (count, width) {
        (Some(count), None) => count as usize,
        (None, Some(width)) => fit(available, width, gap),
        (Some(count), Some(width)) => (count as usize).min(fit(available, width, gap)),
        (None, None) => 1,
    }
    .clamp(1, MAX_COLUMNS);

    // Algebraically the spec's `((available + gap) / n) - gap`.
    let width = ((available - (n as f32 - 1.0) * gap) / n as f32).max(0.0);
    (n, width)
}

/// The flow-space `y` offsets at which a column may break, in a **fixed
/// structural order** (see the module docs: the caller stores indices into this
/// list and re-derives it in the rounded space). Always starts at the flow's
/// top and ends at its bottom.
///
/// Class-A break points (CSS Fragmentation §3): between sibling block-level
/// boxes — the top *border* edge of each in-flow block child, so a preceding
/// margin stays with the previous column — and between the line boxes of an
/// inline formatting context.
///
/// Monolithic content offers no interior break point: the walk does not descend
/// into a replaced box, a table/flex/grid container, a nested multicol root, or
/// a scroll container. Such a box contributes only its own top edge, and the
/// fill treats it as one unbreakable chunk.
///
/// `rounded` selects the coordinate space: `final_layout` (what paint and
/// geometry see) or `unrounded_layout` (what the compute pass has).
pub(crate) fn break_opportunities(tree: &LayoutTree, flow: BoxId, rounded: bool) -> Vec<f32> {
    fn layout_of(b: &LayoutBox, rounded: bool) -> taffy::Layout {
        if rounded {
            b.final_layout
        } else {
            b.unrounded_layout
        }
    }

    fn is_in_flow(b: &LayoutBox) -> bool {
        b.style.position != taffy::Position::Absolute && b.style.float == taffy::Float::None
    }

    /// Whether a column break may be taken *inside* `b`. A box that is not a
    /// block container in normal flow is monolithic: it has no class-A break
    /// points of its own, and slicing it would cut a table row or a flex line.
    fn is_breakable(b: &LayoutBox) -> bool {
        b.multicol.is_none()
            && !matches!(b.kind, BoxKind::Replaced | BoxKind::TableRoot)
            && b.style.display == taffy::Display::Block
            && b.style.overflow.x == Overflow::Visible
            && b.style.overflow.y == Overflow::Visible
    }

    fn collect(tree: &LayoutTree, id: BoxId, y: f32, rounded: bool, out: &mut Vec<f32>) {
        let b = tree.box_(id);
        let layout = layout_of(b, rounded);

        // The line boxes of this box's own IFC. Parley coordinates are relative
        // to the content box, exactly as `paint::text::paint_ifc` places glyphs.
        if let Some(ifc) = b.ifc.as_ref() {
            let content_y = y + layout.border.top + layout.padding.top;
            for line in ifc.layout.lines() {
                out.push(content_y + line.metrics().block_min_coord);
            }
        }

        for &child in &b.children {
            let cb = tree.box_(child);
            if !is_in_flow(cb) {
                continue;
            }
            let child_y = y + layout_of(cb, rounded).location.y;
            out.push(child_y);
            if is_breakable(cb) {
                collect(tree, child, child_y, rounded, out);
            }
        }
    }

    let mut out = vec![0.0f32];
    collect(tree, flow, 0.0, rounded, &mut out);
    out.push(layout_of(tree.box_(flow), rounded).size.height);
    out
}

/// A break opportunity: its flow-space offset and its index in the structural
/// list [`break_opportunities`] returns.
type Opportunity = (f32, usize);

/// The break opportunities sorted by offset, with near-duplicates collapsed.
/// The retained indices still address the structural list.
fn sorted_opportunities(raw: &[f32]) -> Vec<Opportunity> {
    let mut sorted: Vec<Opportunity> = raw.iter().copied().zip(0..).collect();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    sorted.dedup_by(|a, b| (a.0 - b.0).abs() < EPS);
    sorted
}

/// Greedily fills columns of height `h`: each column ends at the last break
/// opportunity at or before its bottom. Returns the column boundaries
/// (`columns + 1` of them, first at the flow's top and last at its bottom).
///
/// Terminates because every step advances to a *strictly greater* opportunity
/// and `ops` is finite.
///
/// Monolithic content taller than `h` has no opportunity inside the column: the
/// fill takes the next one anyway and the column overflows, rather than looping
/// forever. [`balance`] then floors the container height by that chunk, which is
/// what CSS Multicol §3.3 requires.
fn fill(ops: &[Opportunity], h: f32) -> Vec<Opportunity> {
    let (Some(&first), Some(&last)) = (ops.first(), ops.last()) else {
        return vec![(0.0, 0), (0.0, 0)];
    };

    let mut bounds = vec![first];
    let mut cursor = 0usize;
    while ops[cursor].0 < last.0 - EPS && bounds.len() <= ops.len() {
        let start = ops[cursor].0;
        let bottom = start + h;
        let next = ops[cursor + 1..]
            .iter()
            .position(|op| op.0 > bottom + EPS)
            .map_or(ops.len() - 1, |offset| cursor + offset);
        let next = if next > cursor {
            next
        } else {
            // Nothing fits: an unbreakable chunk taller than `h`. Overflow the
            // column rather than stall.
            cursor + 1
        };
        bounds.push(ops[next]);
        cursor = next;
    }
    if bounds.last().map(|op| op.1) != Some(last.1) {
        bounds.push(last);
    }
    if bounds.len() < 2 {
        bounds.push(last);
    }
    bounds
}

/// The column height for at most `n` columns: the smallest `h` a greedy fill
/// needs (by binary search — [`fill`] produces monotonically fewer columns as
/// `h` grows), snapped afterwards to the tallest column the fill actually
/// produced, so the height is achievable rather than a bisection artifact and
/// the container is exactly as tall as its tallest column.
fn balance(ops: &[Opportunity], n: usize) -> (f32, Vec<Opportunity>) {
    let first = ops.first().copied().unwrap_or((0.0, 0));
    let last = ops.last().copied().unwrap_or((0.0, 0));
    let total = last.0 - first.0;
    if n <= 1 || total <= EPS || ops.len() < 2 {
        return (total.max(0.0), vec![first, last]);
    }

    // `fill(total)` always yields a single column, so `hi` is a valid answer.
    let (mut lo, mut hi) = (0.0f32, total);
    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);
        if fill(ops, mid).len() - 1 <= n {
            hi = mid;
        } else {
            lo = mid;
        }
    }

    // The bisection lands on an arbitrary real, but the achievable column
    // heights are differences of break opportunities — a discrete set. Right at
    // the boundary the greedy fill degenerates (`h` is a hair too small for one
    // column but, once `EPS` is added, just large enough for the *next* one to
    // swallow the whole rest), so it fits in `n` columns while being wildly
    // unbalanced. Snap up to the tallest column it produced — an achievable
    // height, and still a fitting one, because `fill` yields no more columns as
    // `h` grows — and fill again. That second fill is the balanced one.
    let tallest = |bounds: &[Opportunity]| {
        bounds
            .windows(2)
            .map(|pair| pair[1].0 - pair[0].0)
            .fold(0.0f32, f32::max)
    };
    let bounds = fill(ops, tallest(&fill(ops, hi)));
    (tallest(&bounds), bounds)
}

/// Maps a point in the continuous-flow space of `flow` into its multicol
/// container's content-box space, by finding the column whose slice shows `y`.
/// A `y` past the last column clamps to it (content that overflowed).
///
/// The single definition of the column transform: paint's per-column
/// `translate(x, -start)`, geometry, and hit-testing all agree with it.
#[must_use]
pub(crate) fn map_flow_point(mc: &MulticolContext, x: f32, y: f32) -> (f32, f32) {
    let column = mc
        .columns
        .iter()
        .rev()
        .find(|column| y >= column.start - EPS)
        .or_else(|| mc.columns.first());
    match column {
        Some(column) => (x + column.x, y - column.start),
        None => (x, y),
    }
}

/// The inverse of [`map_flow_point`]: a point in the container's content-box
/// space back to the flow point shown there, or `None` when it falls in a column
/// gap (or past the last column).
#[must_use]
pub(crate) fn unmap_content_point(mc: &MulticolContext, x: f32, y: f32) -> Option<(f32, f32)> {
    let column = mc.columns.iter().find(|column| {
        x >= column.x && x < column.x + mc.used_width && y >= 0.0 && y < column.end - column.start
    })?;
    Some((x - column.x, y + column.start))
}

/// Turns each multicol container's boundary *indices* into pixel ranges, in the
/// rounded coordinate space paint and geometry read (see the module docs).
///
/// Runs after `taffy::round_layout` on every reflow — including one whose taffy
/// caches all hit, where the compute arm never ran. That is safe and necessary:
/// the indices describe a structure that only a rebuild or a cache-clearing
/// patch can change, and re-deriving the pixels from the current `final_layout`
/// is idempotent.
pub(crate) fn resolve_columns(tree: &mut LayoutTree) {
    for index in 0..tree.box_count() {
        let id = BoxId(index as u32);
        let Some(mc) = tree.box_(id).multicol.as_deref() else {
            continue;
        };
        if mc.boundaries.len() < 2 {
            continue;
        }

        let raw = break_opportunities(tree, mc.flow, /* rounded */ true);
        let mc = tree.box_mut(id).multicol.as_deref_mut().expect("checked");
        let pitch = mc.used_width + mc.used_gap;
        mc.columns = mc
            .boundaries
            .windows(2)
            .enumerate()
            .map(|(k, pair)| ColumnRange {
                // Rounded so every column puts its content on the same pixel
                // grid: the clip and the layer translate both use this value, so
                // they stay in agreement either way, but an unrounded offset
                // would give each column a different subpixel phase.
                x: (k as f32 * pitch).round(),
                start: raw.get(pair[0]).copied().unwrap_or(0.0),
                end: raw.get(pair[1]).copied().unwrap_or(0.0),
            })
            .collect();
    }
}

impl LayoutTree {
    /// Lays out a multi-column container: its content flow once, at the used
    /// column width with an unbounded block size, then sliced into columns.
    pub(crate) fn compute_multicol_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: LayoutInput,
    ) -> LayoutOutput {
        let box_id = BoxId::from(node_id);
        let (config, flow, font_size) = {
            let b = self.box_(box_id);
            let mc = b
                .multicol
                .as_deref()
                .expect("MulticolRoot without a multicol context");
            (mc.config.clone(), mc.flow, b.font_size)
        };

        // --- This box's own frame (the prologue of `compute_inline_layout`). ---
        let style = &self.box_(box_id).style;
        let padding = style
            .padding()
            .resolve_or_zero(inputs.parent_size.width, resolve_calc_value);
        let border = style
            .border()
            .resolve_or_zero(inputs.parent_size.width, resolve_calc_value);
        let padding_border = padding + border;
        let padding_border_size = padding_border.sum_axes();
        let box_sizing_adjustment = if style.box_sizing() == BoxSizing::ContentBox {
            padding_border_size
        } else {
            Size::ZERO
        };
        let scrollbar_gutter = style.overflow().transpose().map(|overflow| match overflow {
            Overflow::Scroll => style.scrollbar_width(),
            _ => 0.0,
        });
        let mut inset = padding_border;
        inset.right += scrollbar_gutter.x;
        inset.bottom += scrollbar_gutter.y;

        let (node_size, min_size, max_size, _) = crate::inline::resolve_node_sizes(
            style,
            inputs.known_dimensions,
            inputs.parent_size,
            inputs.sizing_mode,
            box_sizing_adjustment,
        );

        // Fully determined by style: no need to touch the content at all.
        if inputs.run_mode == RunMode::ComputeSize
            && let Size {
                width: Some(width),
                height: Some(height),
            } = inputs.known_dimensions.or(node_size)
        {
            return LayoutOutput::from_outer_size(Size { width, height });
        }

        // --- The used inline size. -----------------------------------------
        // An in-flow block child arrives with a known width (taffy stretches it),
        // so the intrinsic branch only runs for a float / inline-block / flex
        // item being measured.
        let outer_width = inputs
            .known_dimensions
            .width
            .or(node_size.width)
            .unwrap_or_else(|| {
                let intrinsic = self.measure_multicol_intrinsic(
                    flow,
                    &config,
                    font_size,
                    inputs.available_space.width,
                );
                (match inputs.available_space.width {
                    AvailableSpace::Definite(limit) => limit.min(intrinsic),
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => intrinsic,
                }) + inset.horizontal_axis_sum()
            })
            .maybe_clamp(min_size.width, max_size.width);
        let content_width = (outer_width - inset.horizontal_axis_sum()).max(0.0);

        // --- §3.4: the used gap, count and width. ---------------------------
        let gap = config.used_gap(content_width, font_size);
        let (_, width) = used_columns(content_width, config.count, config.width, gap);

        // --- Lay the flow out once, at width W, with an unbounded block size. -
        // No re-fragmentation is needed afterwards: balancing only chooses slice
        // boundaries in an already-laid-out flow. That is what makes this cheap.
        let flow_output = self.compute_child_layout(
            flow.into(),
            LayoutInput {
                run_mode: RunMode::PerformLayout,
                sizing_mode: SizingMode::InherentSize,
                axis: RequestedAxis::Both,
                known_dimensions: Size {
                    width: Some(width),
                    height: None,
                },
                parent_size: Size {
                    width: Some(width),
                    height: None,
                },
                available_space: Size {
                    width: AvailableSpace::Definite(width),
                    height: AvailableSpace::MaxContent,
                },
                vertical_margins_are_collapsible: taffy::Line::FALSE,
            },
        );
        let flow_height = flow_output.size.height;

        // The flow sits at the container's content-box origin; a column's `x` is
        // measured from there.
        self.set_unrounded_layout(
            flow.into(),
            &taffy::Layout {
                order: 0,
                location: Point {
                    x: inset.left,
                    y: inset.top,
                },
                size: Size {
                    width,
                    height: flow_height,
                },
                content_size: flow_output.content_size,
                scrollbar_size: Size::ZERO,
                border: taffy::Rect::ZERO,
                padding: taffy::Rect::ZERO,
                margin: taffy::Rect::ZERO,
            },
        );

        // --- Slice it. -------------------------------------------------------
        let raw = break_opportunities(self, flow, /* rounded */ false);
        let ops = sorted_opportunities(&raw);
        let definite_content_height = inputs
            .known_dimensions
            .height
            .or(node_size.height)
            .map(|height| (height - inset.vertical_axis_sum()).max(0.0));
        let (height, bounds) = match definite_content_height {
            // `column-fill` does not exist in this stylo build, so a definite
            // block size behaves as `column-fill: auto`: fill each column to that
            // height instead of balancing.
            Some(height) => (height, fill(&ops, height)),
            None => {
                let (count, _) = used_columns(content_width, config.count, config.width, gap);
                balance(&ops, count)
            }
        };

        {
            let mc = self
                .box_mut(box_id)
                .multicol
                .as_deref_mut()
                .expect("checked above");
            mc.used_width = width;
            mc.used_gap = gap;
            mc.boundaries = bounds.iter().take(MAX_COLUMNS + 1).map(|op| op.1).collect();
        }

        let size = Size {
            width: outer_width,
            height: height + inset.vertical_axis_sum(),
        }
        .maybe_clamp(min_size, max_size)
        .maybe_max(padding_border_size.map(Some));

        LayoutOutput {
            size,
            // The flow is taller than the box by construction; letting that reach
            // `content_size` would report the un-columnized height as scrollable
            // overflow (the same cap `BoxKind::TableRoot` applies).
            content_size: size,
            first_baselines: Point {
                x: None,
                y: flow_output.first_baselines.y.map(|b| b + inset.top),
            },
            // A multicol container establishes a formatting context: nothing
            // collapses through it.
            top_margin: CollapsibleMarginSet::ZERO,
            bottom_margin: CollapsibleMarginSet::ZERO,
            margins_can_collapse_through: false,
        }
    }

    /// The container's intrinsic content inline size (CSS Multicol §9,
    /// approximated): min-content is one column, max-content is the preferred
    /// column count side by side.
    fn measure_multicol_intrinsic(
        &mut self,
        flow: BoxId,
        config: &MulticolConfig,
        font_size: f32,
        space: AvailableSpace,
    ) -> f32 {
        let flow_width = self
            .compute_child_layout(
                flow.into(),
                LayoutInput {
                    run_mode: RunMode::ComputeSize,
                    sizing_mode: SizingMode::InherentSize,
                    axis: RequestedAxis::Horizontal,
                    known_dimensions: Size::NONE,
                    parent_size: Size::NONE,
                    available_space: Size {
                        width: space,
                        height: AvailableSpace::MaxContent,
                    },
                    vertical_margins_are_collapsible: taffy::Line::FALSE,
                },
            )
            .size
            .width;

        // A percentage gap resolves against the container's own inline size,
        // which is what we are computing: treat it as zero, as CSS does for
        // circular percentage bases.
        let gap = config.used_gap(0.0, font_size);
        let column = config.width.unwrap_or(flow_width);
        match space {
            // One column wide: the narrowest the container can be.
            AvailableSpace::MinContent => column.min(flow_width.max(0.0)),
            _ => {
                let count = config.count.unwrap_or(1).min(MAX_COLUMNS as u32) as f32;
                count * column + (count - 1.0) * gap
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[0, 10, 20, 30, 40]` — five break opportunities 10px apart.
    fn ops() -> Vec<Opportunity> {
        sorted_opportunities(&[0.0, 10.0, 20.0, 30.0, 40.0])
    }

    #[test]
    fn used_columns_from_count() {
        assert_eq!(used_columns(300.0, Some(3), None, 0.0), (3, 100.0));
        assert_eq!(used_columns(320.0, Some(2), None, 20.0), (2, 150.0));
    }

    #[test]
    fn used_columns_from_width() {
        // floor((320 + 20) / (100 + 20)) = 2 columns of (320 - 20) / 2 = 150.
        assert_eq!(used_columns(320.0, None, Some(100.0), 20.0), (2, 150.0));
        // The count caps the width-derived fit.
        assert_eq!(used_columns(320.0, Some(1), Some(100.0), 20.0), (1, 320.0));
    }

    #[test]
    fn used_columns_clamps_degenerate_inputs() {
        assert_eq!(used_columns(0.0, None, Some(0.0), 0.0).0, MAX_COLUMNS);
        assert_eq!(used_columns(100.0, Some(1000), None, 0.0).0, MAX_COLUMNS);
        assert_eq!(used_columns(100.0, None, None, 0.0), (1, 100.0));
    }

    #[test]
    fn balance_splits_evenly() {
        let (height, bounds) = balance(&ops(), 2);
        assert_eq!(height, 20.0);
        assert_eq!(
            bounds.iter().map(|op| op.0).collect::<Vec<_>>(),
            [0.0, 20.0, 40.0]
        );
    }

    #[test]
    fn balance_never_breaks_between_opportunities() {
        // Three columns over four 10px chunks: 40 / 3 = 13.3 is not a break
        // opportunity, so the columns land on 20/30/40, not on thirds.
        let (_, bounds) = balance(&ops(), 3);
        for op in &bounds {
            assert!(op.0 % 10.0 == 0.0, "{op:?} is not a break opportunity");
        }
        assert!(bounds.len() - 1 <= 3);
    }

    #[test]
    fn fill_overflows_rather_than_stalling_on_unbreakable_content() {
        // One 100px chunk with no interior break, asked for 10px columns.
        let ops = sorted_opportunities(&[0.0, 100.0]);
        let bounds = fill(&ops, 10.0);
        assert_eq!(
            bounds.iter().map(|op| op.0).collect::<Vec<_>>(),
            [0.0, 100.0]
        );
    }

    #[test]
    fn balance_floors_the_height_by_the_tallest_unbreakable_chunk() {
        let ops = sorted_opportunities(&[0.0, 100.0]);
        let (height, bounds) = balance(&ops, 3);
        assert_eq!(height, 100.0);
        assert_eq!(bounds.len() - 1, 1);
    }

    #[test]
    fn balance_of_an_empty_flow_is_one_empty_column() {
        let ops = sorted_opportunities(&[0.0, 0.0]);
        let (height, bounds) = balance(&ops, 3);
        assert_eq!(height, 0.0);
        assert_eq!(bounds.len(), 2);
    }
}
