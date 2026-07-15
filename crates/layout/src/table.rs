//! Tables laid out as CSS grid (adapted from blitz-dom `layout/table.rs`,
//! WP-M): rows/cells are collected into a [`TableContext`] whose grid-item
//! styles carry the row/column placement (col/rowspan aware); the actual
//! layout runs taffy's grid algorithm through a [`TableTreeWrapper`] that
//! maps wrapper indices back to the cell boxes.
//!
//! Row and row-group elements generate no boxes of their own in v1 (their
//! geometry APIs report nothing; ADR-0006).

use std::rc::Rc;

use style::Atom;
use style::computed_values::border_collapse::T as BorderCollapse;
use style::computed_values::table_layout::T as TableLayout;
use taffy::{LayoutPartialTree as _, ResolveOrZero, TrackSizingFunction, style_helpers};

use crate::construct::taffy_style_for;
use crate::taffy_impl::resolve_calc_value;
use crate::tree::{BoxId, LayoutTree};

/// Grid translation of one table: the synthesized grid container style and
/// the per-cell grid-item styles.
pub struct TableContext {
    pub(crate) style: taffy::Style<Atom>,
    pub(crate) cells: Vec<TableCell>,
}

impl std::fmt::Debug for TableContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableContext")
            .field("cells", &self.cells.len())
            .finish_non_exhaustive()
    }
}

pub(crate) struct TableCell {
    /// The cell's box in the main tree (also a child of the table root box).
    pub(crate) box_id: BoxId,
    /// The grid-item style (placement + collapsed borders).
    pub(crate) style: taffy::Style<Atom>,
}

/// Adapter presenting the table's cells as grid items to taffy while
/// delegating child layout to the main [`LayoutTree`].
pub(crate) struct TableTreeWrapper<'t> {
    pub(crate) tree: &'t mut LayoutTree,
    pub(crate) ctx: Rc<TableContext>,
}

pub(crate) struct RangeIter(std::ops::Range<usize>);

impl Iterator for RangeIter {
    type Item = taffy::NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(taffy::NodeId::from)
    }
}

impl taffy::TraversePartialTree for TableTreeWrapper<'_> {
    type ChildIter<'a>
        = RangeIter
    where
        Self: 'a;

    #[inline(always)]
    fn child_ids(&self, _node_id: taffy::NodeId) -> Self::ChildIter<'_> {
        RangeIter(0..self.ctx.cells.len())
    }

    #[inline(always)]
    fn child_count(&self, _node_id: taffy::NodeId) -> usize {
        self.ctx.cells.len()
    }

    #[inline(always)]
    fn get_child_id(&self, _node_id: taffy::NodeId, index: usize) -> taffy::NodeId {
        index.into()
    }
}
impl taffy::TraverseTree for TableTreeWrapper<'_> {}

impl taffy::LayoutPartialTree for TableTreeWrapper<'_> {
    type CoreContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type CustomIdent = Atom;

    fn get_core_container_style(&self, _node_id: taffy::NodeId) -> &taffy::Style<Atom> {
        &self.ctx.style
    }

    fn resolve_calc_value(&self, calc_ptr: *const (), parent_size: f32) -> f32 {
        resolve_calc_value(calc_ptr, parent_size)
    }

    fn set_unrounded_layout(&mut self, node_id: taffy::NodeId, layout: &taffy::Layout) {
        let box_id = self.ctx.cells[usize::from(node_id)].box_id;
        self.tree.set_unrounded_layout(box_id.into(), layout);
    }

    fn compute_child_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: taffy::tree::LayoutInput,
    ) -> taffy::LayoutOutput {
        let box_id = self.ctx.cells[usize::from(node_id)].box_id;
        self.tree.compute_child_layout(box_id.into(), inputs)
    }
}

impl taffy::LayoutGridContainer for TableTreeWrapper<'_> {
    type GridContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type GridItemStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    fn get_grid_container_style(&self, node_id: taffy::NodeId) -> Self::GridContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_grid_child_style(&self, child_node_id: taffy::NodeId) -> Self::GridItemStyle<'_> {
        &self.ctx.cells[usize::from(child_node_id)].style
    }
}

/// State threaded through the cell-collection walk.
pub(crate) struct TableBuildState {
    pub(crate) is_fixed: bool,
    pub(crate) border_collapse: BorderCollapse,
    pub(crate) row: u32,
    pub(crate) col: u32,
    pub(crate) cells: Vec<TableCell>,
    pub(crate) columns: Vec<TrackSizingFunction>,
    /// Border widths of the first cell (drives the collapsed-border gap).
    pub(crate) first_cell_border: Option<(f32, f32)>,
}

impl TableBuildState {
    pub(crate) fn new(style: &style::properties::ComputedValues) -> Self {
        Self {
            is_fixed: matches!(style.clone_table_layout(), TableLayout::Fixed),
            border_collapse: style.clone_border_collapse(),
            row: 0,
            col: 0,
            cells: Vec::new(),
            columns: Vec::new(),
            first_cell_border: None,
        }
    }

    /// Finalizes the grid container style from the collected cells
    /// (blitz-dom `build_table_context`).
    pub(crate) fn finish(
        mut self,
        table_style: &style::properties::ComputedValues,
    ) -> TableContext {
        let mut style = taffy_style_for(table_style);
        style.item_is_table = true;
        // Use `dense` row-flow so that each cell scans the row from its
        // leftmost column for the first free track; cells then automatically
        // skip columns occupied by rowspan cells from earlier rows.
        style.grid_auto_flow = taffy::GridAutoFlow::RowDense;
        style.grid_auto_columns = Vec::new();
        style.grid_auto_rows = Vec::new();

        self.columns
            .resize(self.col as usize, style_helpers::auto());
        style.grid_template_columns = self.columns.drain(..).map(|dim| dim.into()).collect();
        style.grid_template_rows = vec![style_helpers::auto(); self.row as usize];

        let border_spacing = table_style.clone_border_spacing().0;
        style.gap = match self.border_collapse {
            BorderCollapse::Separate => taffy::Size {
                width: style_helpers::length(border_spacing.width.px()),
                height: style_helpers::length(border_spacing.height.px()),
            },
            BorderCollapse::Collapse => self
                .first_cell_border
                .map(|(x, y)| taffy::Size {
                    width: style_helpers::length(x),
                    height: style_helpers::length(y),
                })
                .unwrap_or(taffy::Size::ZERO.map(style_helpers::length)),
        };

        if self.border_collapse == BorderCollapse::Collapse {
            style.border = taffy::Rect {
                left: style.gap.width,
                right: style.gap.width,
                top: style.gap.height,
                bottom: style.gap.height,
            };
        }

        TableContext {
            style,
            cells: self.cells,
        }
    }

    /// Registers a cell: derives its grid-item style (blitz-dom
    /// `collect_table_cells`, `TableCell` arm).
    pub(crate) fn push_cell(
        &mut self,
        box_id: BoxId,
        cell_style: &style::properties::ComputedValues,
        colspan: u16,
        rowspan: u16,
    ) {
        let mut style = taffy_style_for(cell_style);

        if self.first_cell_border.is_none() {
            // A border with style `none`/`hidden` has zero used width
            // (stylo keeps the specified width in the computed struct).
            use style::values::specified::border::BorderStyle;
            let border = cell_style.get_border();
            let used = |width: app_units::Au, side_style: BorderStyle| -> f32 {
                match side_style {
                    BorderStyle::None | BorderStyle::Hidden => 0.0,
                    _ => width.to_f32_px(),
                }
            };
            let x = used(border.border_left_width.0, border.border_left_style)
                .max(used(border.border_right_width.0, border.border_right_style));
            let y = used(border.border_top_width.0, border.border_top_style).max(used(
                border.border_bottom_width.0,
                border.border_bottom_style,
            ));
            self.first_cell_border = Some((x, y));
        }

        // First row defines the column tracks.
        if self.row == 1 {
            let column = match style.size.width.tag() {
                taffy::CompactLength::LENGTH_TAG => {
                    let len = style.size.width.value();
                    let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
                    style_helpers::length(len + padding.left + padding.right)
                }
                taffy::CompactLength::PERCENT_TAG => {
                    if self.is_fixed {
                        style_helpers::percent(style.size.width.value())
                    } else {
                        style_helpers::auto()
                    }
                }
                _ => style_helpers::auto(),
            };
            self.columns.push(column);
        }

        // Zero out cell borders if border-collapse is Collapse: borders are
        // handled at the table level in this mode.
        if self.border_collapse == BorderCollapse::Collapse {
            style.border = taffy::Rect::ZERO.map(style_helpers::length);
        }

        // Let taffy auto-place the column (dense flow handles rowspan gaps).
        style.grid_column = taffy::Line {
            start: style_helpers::auto(),
            end: style_helpers::span(colspan),
        };
        style.grid_row = taffy::Line {
            // Row indices beyond i16 saturate (taffy grid lines are i16);
            // a >32k-row table degrades gracefully instead of wrapping.
            start: style_helpers::line(self.row.min(i16::MAX as u32) as i16),
            end: style_helpers::span(rowspan),
        };
        style.size.width = style_helpers::auto();

        self.cells.push(TableCell { box_id, style });
        self.col += u32::from(colspan);
    }
}
