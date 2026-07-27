//! Geometry queries for CSSOM-View APIs (`getBoundingClientRect`,
//! `getClientRects`, `offset*`/`client*`/`scroll*`, `elementFromPoint`) plus
//! used-value access for `getComputedStyle` resolved values.
//!
//! Positions are computed by walking up the box tree summing
//! `final_layout.location`, subtracting ancestor scroll offsets — no cached
//! absolute coordinates. Transforms and writing modes are ignored (ADR-0006
//! §6). Callers must reflow first (the bindings' `flush_layout` does).

use oxidepage_base::{NodeId, Point, Rect};
use oxidepage_dom::{DomTree, NodeKind};
use style::computed_values::position::T as Position;

use crate::engine::LayoutEngine;
use crate::scroll::ScrollResult;
use crate::tree::{BoxId, PseudoBox};

/// `clientLeft`/`clientTop`/`clientWidth`/`clientHeight`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClientBox {
    /// Left border width.
    pub left: f32,
    /// Top border width.
    pub top: f32,
    /// Padding-box width.
    pub width: f32,
    /// Padding-box height.
    pub height: f32,
}

/// `offsetParent`/`offsetLeft`/`offsetTop`/`offsetWidth`/`offsetHeight`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OffsetBox {
    pub parent: Option<NodeId>,
    pub left: f32,
    pub top: f32,
    /// Border-box width.
    pub width: f32,
    /// Border-box height.
    pub height: f32,
}

/// Resolution of `Element.scrollParent()` (CSSOM-View, draft): either no
/// scroll parent, a concrete scroll-container ancestor, or "the
/// containing-block walk reached the initial containing block" — which the
/// caller resolves to `document.scrollingElement`. Layout has no notion of
/// scrolling-element promotion (quirks mode moves it to `<body>`), so that
/// resolution is left to the DOM/bindings layer that already implements it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollParent {
    None,
    Element(NodeId),
    DocumentScrollingElement,
}

/// The used values of `top`/`right`/`bottom`/`left`, whose resolved value is a
/// *different thing* for each `position` (CSSOM "resolved value", and CSS
/// Position §3; WPT's `getComputedStyle-insets-*` pins every case):
///
/// * `static` — the property does not apply. The resolved value is the computed
///   one, percentages and `auto` and all: `None` here, so the caller reports it.
/// * `relative` — the used value is the offset relative positioning *applied*,
///   and that follows from the specified insets alone: percentages against the
///   containing block, then `auto` on one side is the negative of the other, and
///   `auto` on both is zero. It must **not** be read off the box's position —
///   that would fold in where the box sits in flow, which is not an inset.
/// * `sticky` — as `relative` for lengths and percentages, but `auto` stays
///   `auto` (there is no offset to report until the box is actually stuck).
/// * `absolute`/`fixed` — the used value is where the box landed, which is what
///   `laid_out` carries (measured from the containing block's *padding* box).
///
/// An **over-constrained** axis is the one exception for absolute/fixed: CSSOM
/// says the resolved value is the used one only "if the property is not
/// over-constrained", so there the computed value (absolutized) is reported
/// instead of where the box actually landed.
///
/// Percentages resolve against the containing block: the parent's **content** box
/// for relative/sticky, its **padding** box for absolute/fixed.
fn used_insets(
    position: Position,
    inset: &taffy::Rect<taffy::LengthPercentageAuto>,
    size: &taffy::Size<taffy::Dimension>,
    cb_content: (f32, f32),
    cb_padding: (f32, f32),
    laid_out: [f32; 4],
) -> [Option<f32>; 4] {
    let resolve = |value: taffy::LengthPercentageAuto, basis: f32| {
        value.resolve_to_option(basis, crate::taffy_impl::resolve_calc_value)
    };
    match position {
        Position::Static => [None; 4],
        Position::Absolute | Position::Fixed => {
            // Over-constrained: both insets given, and either the size is fixed
            // too, or honouring both insets would give the box a negative size.
            let axis = |start: taffy::LengthPercentageAuto,
                        end: taffy::LengthPercentageAuto,
                        size: taffy::Dimension,
                        cb: f32,
                        used_start: f32,
                        used_end: f32|
             -> (Option<f32>, Option<f32>) {
                let (Some(s), Some(e)) = (resolve(start, cb), resolve(end, cb)) else {
                    return (Some(used_start), Some(used_end));
                };
                let over_constrained = !size.is_auto() || (cb - s - e) < 0.0;
                if over_constrained {
                    (Some(s), Some(e))
                } else {
                    (Some(used_start), Some(used_end))
                }
            };
            let (top, bottom) = axis(
                inset.top,
                inset.bottom,
                size.height,
                cb_padding.1,
                laid_out[0],
                laid_out[2],
            );
            let (left, right) = axis(
                inset.left,
                inset.right,
                size.width,
                cb_padding.0,
                laid_out[3],
                laid_out[1],
            );
            [top, right, bottom, left]
        }
        Position::Relative | Position::Sticky => {
            // Per axis: (start, end) = (top, bottom) and (left, right).
            let axis = |start: Option<f32>, end: Option<f32>| -> (Option<f32>, Option<f32>) {
                if position == Position::Sticky {
                    // `auto` is preserved; a length or percentage is absolutized.
                    return (start, end);
                }
                match (start, end) {
                    (Some(s), Some(e)) => (Some(s), Some(e)),
                    (Some(s), None) => (Some(s), Some(-s)),
                    (None, Some(e)) => (Some(-e), Some(e)),
                    (None, None) => (Some(0.0), Some(0.0)),
                }
            };
            let (top, bottom) = axis(
                resolve(inset.top, cb_content.1),
                resolve(inset.bottom, cb_content.1),
            );
            let (left, right) = axis(
                resolve(inset.left, cb_content.0),
                resolve(inset.right, cb_content.0),
            );
            [top, right, bottom, left]
        }
    }
}

/// Used (post-layout) box values for `getComputedStyle` resolved values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UsedBoxValues {
    /// Content-box size.
    pub width: f32,
    pub height: f32,
    /// Margins: top, right, bottom, left.
    pub margin: [f32; 4],
    /// Padding: top, right, bottom, left.
    pub padding: [f32; 4],
    /// Used inset offsets (top, right, bottom, left). `None` means "there is no
    /// used value" — the resolved value is the *computed* one, which is how
    /// CSSOM treats a static box's insets and an `auto` inset on a sticky box.
    /// (ADR-0006 §5: v1 containing block = direct parent.)
    pub inset: [Option<f32>; 4],
    /// The computed `position`, so callers can keep `auto` for static boxes.
    pub position: Position,
}

impl LayoutEngine {
    /// The multi-column context of the container `box_id` is the flow box of,
    /// if it is one.
    fn multicol_of_flow(&self, box_id: BoxId) -> Option<&crate::multicol::MulticolContext> {
        let root = self.tree().multicol_root_of_flow(box_id)?;
        self.tree().box_(root).multicol.as_deref()
    }

    /// The multi-column frame `box_id` sits in, if any: the enclosing flow box,
    /// that flow's absolute origin, and `box_id`'s border-box offset *within* the
    /// flow's continuous coordinate space.
    ///
    /// Callers that need to place something at a sub-box granularity (a line box
    /// inside an IFC) need this rather than [`Self::absolute_origin`]: the latter
    /// maps the box's *own* origin into the column that shows it, but an IFC — or
    /// an anonymous block — may straddle a column break and put its lines in two
    /// different columns.
    fn multicol_frame(&self, box_id: BoxId, include_scroll: bool) -> Option<(BoxId, Point, Point)> {
        let mut offset = Point::ZERO;
        let mut current = box_id;
        loop {
            if self.multicol_of_flow(current).is_some() {
                return Some((
                    current,
                    self.absolute_origin(current, include_scroll),
                    offset,
                ));
            }
            let b = self.tree().box_(current);
            let parent = b.parent?;
            offset.x += b.final_layout.location.x;
            offset.y += b.final_layout.location.y;
            if include_scroll {
                let scroll = self.box_scroll_offset(parent);
                offset.x -= scroll.x;
                offset.y -= scroll.y;
            }
            current = parent;
        }
    }

    /// Absolute border-box origin of `box_id` (viewport coordinates when
    /// `include_scroll`; document coordinates ignoring scroll otherwise —
    /// the `offset*` family ignores scrolling).
    fn absolute_origin(&self, box_id: BoxId, include_scroll: bool) -> Point {
        let mut x = 0.0f32;
        let mut y = 0.0f32;
        let mut current = box_id;
        loop {
            let b = self.tree().box_(current);

            // Reaching a multicol flow box from below: `(x, y)` is a position in
            // the continuous flow, so shift it into the column that shows it —
            // the very transform paint applies (ADR-0016). Adding the flow's own
            // location afterwards lands in the container's border box, because
            // the flow sits at the container's content origin and a column's `x`
            // is measured from there.
            if current != box_id
                && let Some(mc) = self.multicol_of_flow(current)
            {
                (x, y) = crate::multicol::map_flow_point(mc, x, y);
            }

            x += b.final_layout.location.x;
            y += b.final_layout.location.y;
            let Some(parent) = b.parent else { break };
            if include_scroll {
                let off = self.box_scroll_offset(parent);
                x -= off.x;
                y -= off.y;
            }
            current = parent;
        }
        if include_scroll {
            let vp = self.viewport_scroll();
            x -= vp.x;
            y -= vp.y;
        }
        Point::new(x, y)
    }

    /// The absolute (viewport-relative) border-box rect of `node`'s
    /// principal box.
    #[must_use]
    pub fn border_box(&self, node: NodeId) -> Option<Rect> {
        let box_id = self.tree().box_for_node(node)?;
        let origin = self.absolute_origin(box_id, true);
        let size = self.tree().box_(box_id).final_layout.size;
        Some(Rect::from_xywh(origin.x, origin.y, size.width, size.height))
    }

    /// The node's **padding box** in document coordinates — the origin
    /// `MouseEvent.offsetX/offsetY` are measured from.
    #[must_use]
    pub fn padding_box(&self, node: NodeId) -> Option<Rect> {
        let box_id = self.tree().box_for_node(node)?;
        let origin = self.absolute_origin(box_id, true);
        let layout = &self.tree().box_(box_id).final_layout;
        let border = layout.border;
        Some(Rect::from_xywh(
            origin.x + border.left,
            origin.y + border.top,
            (layout.size.width - border.left - border.right).max(0.0),
            (layout.size.height - border.top - border.bottom).max(0.0),
        ))
    }

    /// `getClientRects()`: one rect for box-generating elements, one rect
    /// per line fragment for inline elements inside an IFC.
    #[must_use]
    pub fn client_rects(&self, dom: &DomTree, node: NodeId) -> Vec<Rect> {
        if let Some(rect) = self.border_box(node) {
            return vec![rect];
        }
        self.inline_fragment_rects(dom, node)
    }

    /// `getBoundingClientRect()`: the border box, or the union of inline
    /// fragment rects; `None` when the element generates no boxes.
    #[must_use]
    pub fn bounding_client_rect(&self, dom: &DomTree, node: NodeId) -> Option<Rect> {
        let rects = self.client_rects(dom, node);
        let (first, rest) = rects.split_first()?;
        Some(rest.iter().fold(*first, |acc, r| acc.union(r)))
    }

    /// Per-line rects for an inline (non-box-generating) element: the runs
    /// and atomic inline boxes of the nearest ancestor IFC whose brush node
    /// is `node` or a DOM descendant of it.
    fn inline_fragment_rects(&self, dom: &DomTree, node: NodeId) -> Vec<Rect> {
        if dom.node(node).data().kind() != NodeKind::Element {
            return Vec::new();
        }
        // Find the nearest ancestor that generates a box.
        let mut ancestor = None;
        for a in dom.ancestors(node) {
            if let Some(b) = self.tree().box_for_node(a) {
                ancestor = Some(b);
                break;
            }
        }
        let Some(mut ifc_box_id) = ancestor else {
            return Vec::new();
        };
        // In a mixed container the inline run lives in an *anonymous* IFC
        // box: the nearest boxed DOM ancestor is the container, so look for
        // the anonymous descendant whose contributors include `node`.
        if self.tree().box_(ifc_box_id).ifc.is_none() {
            let Some(anon) = self.anonymous_ifc_of(ifc_box_id, node) else {
                return Vec::new();
            };
            ifc_box_id = anon;
        }
        let ifc_box = self.tree().box_(ifc_box_id);
        let Some(ifc) = ifc_box.ifc.as_ref() else {
            return Vec::new();
        };
        if !ifc.contributors.contains(&node) {
            return Vec::new();
        }

        let is_self_or_descendant = |candidate: NodeId| -> bool {
            candidate == node || dom.ancestors(candidate).any(|a| a == node)
        };

        // Content-box origin of the IFC (parley coordinates are relative to it).
        // Inside a multi-column container this stays in the *flow*'s continuous
        // space: a line box is mapped into the column that shows it one at a time
        // (an inline spanning a column break therefore reports one rect per
        // column — a line never straddles a break, because line tops are exactly
        // where the breaks are taken).
        let frame = self.multicol_frame(ifc_box_id, true);
        let (base, offset) = match frame {
            Some((_, flow_origin, offset)) => (flow_origin, offset),
            None => (self.absolute_origin(ifc_box_id, true), Point::ZERO),
        };
        let content_x =
            offset.x + ifc_box.final_layout.border.left + ifc_box.final_layout.padding.left;
        let content_y =
            offset.y + ifc_box.final_layout.border.top + ifc_box.final_layout.padding.top;
        let columns = frame.and_then(|(flow, _, _)| self.multicol_of_flow(flow));

        let place = |rect: Rect| -> Rect {
            let (x, y) = (rect.origin.x + content_x, rect.origin.y + content_y);
            let (x, y) = match columns {
                Some(mc) => crate::multicol::map_flow_point(mc, x, y),
                None => (x, y),
            };
            Rect::new(Point::new(base.x + x, base.y + y), rect.size)
        };

        let mut rects = Vec::new();
        for line in ifc.layout.lines() {
            let metrics = line.metrics();
            let mut line_rect: Option<Rect> = None;
            for item in line.items() {
                let rect = match item {
                    parley::PositionedLayoutItem::GlyphRun(run) => {
                        let brush_node = run.style().brush.node();
                        if !brush_node.is_some_and(is_self_or_descendant) {
                            continue;
                        }
                        Rect::from_xywh(
                            run.offset(),
                            metrics.baseline - metrics.ascent,
                            run.advance(),
                            metrics.ascent + metrics.descent,
                        )
                    }
                    parley::PositionedLayoutItem::InlineBox(ibox) => {
                        let child = self.tree().box_(BoxId::from(taffy::NodeId::from(ibox.id)));
                        let owner = child.dom_node;
                        if !owner.is_some_and(is_self_or_descendant) {
                            continue;
                        }
                        Rect::from_xywh(ibox.x, ibox.y, ibox.width, ibox.height)
                    }
                };
                line_rect = Some(match line_rect {
                    Some(acc) => acc.union(&rect),
                    None => rect,
                });
            }
            if let Some(rect) = line_rect {
                rects.push(place(rect));
            }
        }
        rects
    }

    /// The anonymous descendant box whose IFC lists `node` as a contributor.
    /// Only anonymous boxes are descended into: a *boxed* descendant owning an
    /// IFC that contained `node` would be a nearer boxed ancestor of `node`, so
    /// the search would never have started here. A multicol container puts one
    /// extra anonymous box (its flow) between the container and the anonymous
    /// wrappers, which is why this recurses rather than scanning one level.
    fn anonymous_ifc_of(&self, box_id: BoxId, node: NodeId) -> Option<BoxId> {
        for &child in &self.tree().box_(box_id).children {
            let b = self.tree().box_(child);
            if b.dom_node.is_some() {
                continue;
            }
            if b.ifc
                .as_ref()
                .is_some_and(|ifc| ifc.contributors.contains(&node))
            {
                return Some(child);
            }
            if let Some(found) = self.anonymous_ifc_of(child, node) {
                return Some(found);
            }
        }
        None
    }

    /// The client (padding-box) size of a box; the root box reports the
    /// viewport (CSSOM-View `documentElement` special case). Single source
    /// for `client_box`, `scroll_size`, and scroll clamping.
    fn client_size(&self, box_id: BoxId) -> (f32, f32) {
        if Some(box_id) == self.tree().root() {
            let viewport = self.viewport();
            return (viewport.width, viewport.height);
        }
        let layout = &self.tree().box_(box_id).final_layout;
        (
            layout.size.width - layout.border.left - layout.border.right,
            layout.size.height - layout.border.top - layout.border.bottom,
        )
    }

    /// `clientLeft/Top/Width/Height`. The `documentElement` reports the
    /// viewport size (CSSOM-View special case).
    #[must_use]
    pub fn client_box(&self, node: NodeId) -> Option<ClientBox> {
        let box_id = self.tree().box_for_node(node)?;
        let (width, height) = self.client_size(box_id);
        let (left, top) = if Some(box_id) == self.tree().root() {
            (0.0, 0.0)
        } else {
            let layout = &self.tree().box_(box_id).final_layout;
            (layout.border.left, layout.border.top)
        };
        Some(ClientBox {
            left,
            top,
            width,
            height,
        })
    }

    /// The `offset*` family (CSSOM-View `offsetParent` algorithm, without
    /// the `td/th/table` special cases beyond positioned/body ancestors).
    #[must_use]
    pub fn offset_box(&self, dom: &DomTree, node: NodeId) -> Option<OffsetBox> {
        let box_id = self.tree().box_for_node(node)?;
        let layout_box = self.tree().box_(box_id);
        let size = layout_box.final_layout.size;
        let position = layout_box.position;

        let is_body = |n: NodeId| -> bool {
            dom.node(n)
                .as_element()
                .is_some_and(|el| el.is_html_element() && &*el.name.local == "body")
        };
        let is_root_or_body = |n: NodeId| -> bool {
            dom.node(n).as_element().is_some_and(|el| {
                el.is_html_element() && matches!(&*el.name.local, "html" | "body")
            })
        };

        // offsetParent is null for fixed elements, the root, and the body.
        let parent = if position == Position::Fixed || is_root_or_body(node) {
            None
        } else {
            let mut found = None;
            for a in dom.ancestors(node) {
                let Some(a_box) = self.tree().box_for_node(a) else {
                    continue;
                };
                let a_positioned = self.tree().box_(a_box).position != Position::Static;
                let a_is_table_cellish = dom.node(a).as_element().is_some_and(|el| {
                    el.is_html_element() && matches!(&*el.name.local, "td" | "th" | "table")
                });
                if a_positioned || a_is_table_cellish || is_root_or_body(a) {
                    found = Some(a);
                    break;
                }
            }
            found
        };

        let own = self.absolute_origin(box_id, false);
        let (left, top) = match parent {
            // A statically-positioned body as offsetParent reports offsets
            // relative to the initial containing block, *not* to its own padding
            // edge: the body's margin is included. CSSOM-View's offsetTop step 3
            // says otherwise, but every engine has this legacy carve-out (Blink:
            // `AdjustedPositionRelativeTo`), and WPT is written against it — with
            // the default `body { margin: 8px }`, a plain `<div>` in the body has
            // `offsetTop === 8`, not 0.
            Some(p) if is_body(p) && self.node_position(p) == Some(Position::Static) => {
                (own.x, own.y)
            }
            Some(p) => {
                let p_box = self.tree().box_for_node(p).expect("parent has a box");
                let p_origin = self.absolute_origin(p_box, false);
                let p_layout = &self.tree().box_(p_box).final_layout;
                // Relative to the offsetParent's padding edge.
                (
                    own.x - (p_origin.x + p_layout.border.left),
                    own.y - (p_origin.y + p_layout.border.top),
                )
            }
            None => (own.x, own.y),
        };

        Some(OffsetBox {
            parent,
            left,
            top,
            width: size.width,
            height: size.height,
        })
    }

    /// The content-box rect of `node`'s principal box in **element-local**
    /// coordinates: origin = `(paddingLeft, paddingTop)`, size shrunk by border
    /// and padding (clamped to ≥ 0). This matches `ResizeObserverEntry.contentRect`,
    /// whose `x`/`y` are the padding offsets (not viewport coordinates).
    #[must_use]
    pub fn content_box(&self, node: NodeId) -> Option<Rect> {
        let box_id = self.tree().box_for_node(node)?;
        let layout = &self.tree().box_(box_id).final_layout;
        let width = (layout.size.width
            - layout.border.left
            - layout.border.right
            - layout.padding.left
            - layout.padding.right)
            .max(0.0);
        let height = (layout.size.height
            - layout.border.top
            - layout.border.bottom
            - layout.padding.top
            - layout.padding.bottom)
            .max(0.0);
        Some(Rect::from_xywh(
            layout.padding.left,
            layout.padding.top,
            width,
            height,
        ))
    }

    /// `scrollWidth`/`scrollHeight`: the scrollable-overflow extent, floored
    /// by the padding box. The `documentElement` is floored by the viewport.
    ///
    /// Only the logical-end edge of each axis contributes — the logical-start
    /// edge's negative excess (e.g. from `align-items: center` overflowing a
    /// flex container's cross-start) is unreachable by scrolling and must not
    /// inflate the result (`crate::overflow`'s doc comment and
    /// `logical_end_is_positive`). Which physical edge is the end depends on
    /// `direction` and, for a flex main axis, `flex-direction`'s `-reverse`
    /// variants.
    #[must_use]
    pub fn scroll_size(&self, node: NodeId) -> Option<(f32, f32)> {
        let box_id = self.tree().box_for_node(node)?;
        let (client_w, client_h) = self.client_size(box_id);
        let b = self.tree().box_(box_id);
        let border = b.final_layout.border;
        let size = b.final_layout.size;
        let overflow = b.scrollable_overflow;
        let (x_end_positive, y_end_positive) = crate::overflow::logical_end_is_positive(&b.style);
        let width = if x_end_positive {
            overflow.max_x() - border.left
        } else {
            (size.width - border.right) - overflow.min_x()
        };
        let height = if y_end_positive {
            overflow.max_y() - border.top
        } else {
            (size.height - border.bottom) - overflow.min_y()
        };
        Some((width.max(client_w), height.max(client_h)))
    }

    /// Used box values for resolved `getComputedStyle` properties.
    #[must_use]
    pub fn used_box_values(&self, node: NodeId) -> Option<UsedBoxValues> {
        let box_id = self.tree().box_for_node(node)?;
        let b = self.tree().box_(box_id);
        let layout = &b.final_layout;

        let (parent_padding_box, parent_padding_origin) = match b.parent {
            Some(p) => {
                let pl = &self.tree().box_(p).final_layout;
                (
                    (
                        pl.size.width - pl.border.left - pl.border.right,
                        pl.size.height - pl.border.top - pl.border.bottom,
                    ),
                    (pl.border.left, pl.border.top),
                )
            }
            None => {
                let viewport = self.viewport();
                ((viewport.width, viewport.height), (0.0, 0.0))
            }
        };

        let margin = layout.margin;
        let left = layout.location.x - parent_padding_origin.0 - margin.left;
        let top = layout.location.y - parent_padding_origin.1 - margin.top;
        let right = parent_padding_box.0
            - (layout.location.x - parent_padding_origin.0 + layout.size.width + margin.right);
        let bottom = parent_padding_box.1
            - (layout.location.y - parent_padding_origin.1 + layout.size.height + margin.bottom);

        // The containing block for a *relative*/*sticky* box's percentages is the
        // parent's **content** box, while an absolutely-positioned box resolves
        // against its **padding** box (CSS Position §3; WPT's
        // `getComputedStyle-insets-*` pins both).
        let parent_content_box = match b.parent {
            Some(p) => {
                let pl = &self.tree().box_(p).final_layout;
                (
                    (pl.size.width
                        - pl.border.left
                        - pl.border.right
                        - pl.padding.left
                        - pl.padding.right)
                        .max(0.0),
                    (pl.size.height
                        - pl.border.top
                        - pl.border.bottom
                        - pl.padding.top
                        - pl.padding.bottom)
                        .max(0.0),
                )
            }
            None => parent_padding_box,
        };
        let inset = used_insets(
            b.position,
            &b.style.inset,
            &b.style.size,
            parent_content_box,
            parent_padding_box,
            [top, right, bottom, left],
        );

        Some(UsedBoxValues {
            width: layout.size.width
                - layout.border.left
                - layout.border.right
                - layout.padding.left
                - layout.padding.right,
            height: layout.size.height
                - layout.border.top
                - layout.border.bottom
                - layout.padding.top
                - layout.padding.bottom,
            margin: [margin.top, margin.right, margin.bottom, margin.left],
            padding: [
                layout.padding.top,
                layout.padding.right,
                layout.padding.bottom,
                layout.padding.left,
            ],
            inset,
            position: b.position,
        })
    }

    // === Hit testing ===

    /// `document.elementFromPoint(x, y)` (viewport CSS px).
    #[must_use]
    pub fn element_from_point(&self, dom: &DomTree, x: f32, y: f32) -> Option<NodeId> {
        self.elements_from_point(dom, x, y).into_iter().next()
    }

    /// `document.elementsFromPoint(x, y)`: hit elements in approximate paint
    /// order (topmost first), ending with the document element. Boxes with
    /// `pointer-events: none` are transparent to the test — the point falls
    /// through to whatever is behind them. Doesn't descend into iframes
    /// (ADR-0006 §6).
    #[must_use]
    pub fn elements_from_point(&self, dom: &DomTree, x: f32, y: f32) -> Vec<NodeId> {
        let viewport = self.viewport();
        if x < 0.0 || y < 0.0 || x >= viewport.width || y >= viewport.height {
            return Vec::new();
        }
        let Some(root) = self.tree().root() else {
            return Vec::new();
        };
        let vp = self.viewport_scroll();
        let root_layout = self.tree().box_(root).final_layout;
        let pt = Point::new(
            x + vp.x - root_layout.location.x,
            y + vp.y - root_layout.location.y,
        );

        let mut hits = Vec::new();
        self.hit_box(dom, root, pt, &mut hits);

        // Always report the document element last.
        if let Some(root_node) = self.tree().box_(root).dom_node
            && hits.last() != Some(&root_node)
        {
            hits.push(root_node);
        }
        hits
    }

    /// The nearest DOM element enclosing `box_id`'s content: the box's own node,
    /// or — for an anonymous box, which has none — the node of its nearest
    /// non-anonymous ancestor box.
    fn containing_element(&self, box_id: BoxId) -> Option<NodeId> {
        let mut current = Some(box_id);
        while let Some(id) = current {
            let b = self.tree().box_(id);
            if b.dom_node.is_some() {
                return b.dom_node;
            }
            current = b.parent;
        }
        None
    }

    /// Recursive hit test. `pt` is relative to `box_id`'s border-box origin.
    fn hit_box(&self, dom: &DomTree, box_id: BoxId, pt: Point, out: &mut Vec<NodeId>) {
        let b = self.tree().box_(box_id);
        let size = b.final_layout.size;
        let contains = pt.x >= 0.0 && pt.x < size.width && pt.y >= 0.0 && pt.y < size.height;

        let clips = b.style.overflow.x != taffy::Overflow::Visible
            || b.style.overflow.y != taffy::Overflow::Visible;
        // Overflow clips at the *padding* edge, so a point inside the border
        // strip hits this box but must not reach its clipped children.
        let border = b.final_layout.border;
        let within_clip = pt.x >= border.left
            && pt.x < size.width - border.right
            && pt.y >= border.top
            && pt.y < size.height - border.bottom;
        // A multicol root shows its flow through per-column clipped views:
        // descend into the column the point falls in, in the flow's own
        // coordinate space (ADR-0016). A point in a column gap hits the
        // container but reaches no content.
        if let Some(mc) = b.multicol.as_deref() {
            if !clips || within_clip {
                let flow_location = self.tree().box_(mc.flow()).final_layout.location;
                let local = Point::new(pt.x - flow_location.x, pt.y - flow_location.y);
                if let Some((x, y)) = crate::multicol::unmap_content_point(mc, local.x, local.y) {
                    self.hit_box(dom, mc.flow(), Point::new(x, y), out);
                }
            }
        } else if !clips || within_clip {
            // Children in top-to-bottom paint order: positioned z ≥ 0 above
            // in-flow above positioned z < 0; later siblings above earlier.
            let mut ordered: Vec<(i32, i32, usize, BoxId)> = b
                .children
                .iter()
                .enumerate()
                .map(|(idx, &child)| {
                    let cb = self.tree().box_(child);
                    let positioned = cb.position != Position::Static;
                    let priority = if positioned && cb.z_index >= 0 {
                        2
                    } else if positioned {
                        0
                    } else {
                        1
                    };
                    (priority, cb.z_index, idx, child)
                })
                .collect();
            ordered.sort_by_key(|entry| std::cmp::Reverse((entry.0, entry.1, entry.2)));

            // All hitting siblings contribute (elementsFromPoint reports the
            // full stack, not just the topmost branch), in paint order.
            let scroll = self.box_scroll_offset(box_id);
            for (_, _, _, child) in ordered {
                let child_layout = self.tree().box_(child).final_layout;
                let child_pt = Point::new(
                    pt.x + scroll.x - child_layout.location.x,
                    pt.y + scroll.y - child_layout.location.y,
                );
                self.hit_box(dom, child, child_pt, out);
            }
        }

        if contains {
            // A hit inside an IFC attributes text runs to their span.
            if out.is_empty()
                && let Some(ifc) = b.ifc.as_ref()
            {
                // The element the IFC belongs to. An anonymous block (the wrapper
                // for inline runs beside block siblings, and every flex/grid item's
                // text) has no `dom_node` of its own, so it borrows its nearest
                // non-anonymous ancestor's. Comparing against `b.dom_node` directly
                // made both the guard below and the ancestor-walk terminator
                // degenerate for anonymous boxes: the guard collapsed to
                // `brush_node != brush_node` and skipped attribution entirely.
                let ifc_element = self.containing_element(box_id);
                let content_pt = Point::new(
                    pt.x - b.final_layout.border.left - b.final_layout.padding.left,
                    pt.y - b.final_layout.border.top - b.final_layout.padding.top,
                );
                'lines: for line in ifc.layout.lines() {
                    let metrics = line.metrics();
                    let line_top = metrics.baseline - metrics.ascent;
                    let line_bottom = metrics.baseline + metrics.descent;
                    if content_pt.y < line_top || content_pt.y >= line_bottom {
                        continue;
                    }
                    for item in line.items() {
                        if let parley::PositionedLayoutItem::GlyphRun(run) = item
                            && content_pt.x >= run.offset()
                            && content_pt.x < run.offset() + run.advance()
                            && let Some(brush_node) = run.style().brush.node()
                            && Some(brush_node) != ifc_element
                        {
                            // Push the span and its element ancestors up to
                            // (excluding) the IFC's element, which the enclosing
                            // frames push themselves.
                            let mut chain = vec![brush_node];
                            for a in dom.ancestors(brush_node) {
                                if Some(a) == ifc_element {
                                    break;
                                }
                                if dom.node(a).data().kind() == NodeKind::Element {
                                    chain.push(a);
                                }
                            }
                            out.extend(chain);
                            break 'lines;
                        }
                    }
                }
            }

            // A `::before`/`::after` box lies inside its owner's principal box,
            // which reports the hit itself. A list marker is the exception: an
            // outside marker sits *beyond* the item's border box, so the item's
            // own box never contains the point and the marker has to report it
            // (CSS-UI: the marker is part of the list item for hit-testing). The
            // dedup guard covers the overlap case — an item with enough
            // `padding-left` to swallow its own marker.
            let reports_hit = match b.pseudo {
                None | Some(PseudoBox::Marker) => true,
                Some(PseudoBox::Before | PseudoBox::After) => false,
            };
            // `pointer-events: none` makes the box transparent to hit testing:
            // the point falls through to whatever is behind it. Descendants are
            // *not* excluded — the property is inherited, so a child that sets
            // `pointer-events: auto` back is hit normally, and the recursion
            // above has already visited them.
            if let Some(node) = b.dom_node
                && reports_hit
                && !b.pointer_events_none
                && dom.node(node).data().kind() == NodeKind::Element
                && out.last() != Some(&node)
            {
                out.push(node);
            }
        }
    }

    // === Scroll offsets ===

    /// The clamped scroll offset of `node` (0,0 for non-scroll-containers).
    #[must_use]
    pub fn scroll_offset(&self, node: NodeId) -> Point {
        match self.tree().box_for_node(node) {
            Some(box_id) => self.box_scroll_offset(box_id),
            None => Point::ZERO,
        }
    }

    fn box_scroll_offset(&self, box_id: BoxId) -> Point {
        let Some(node) = self.tree().box_(box_id).dom_node else {
            return Point::ZERO;
        };
        let Some(&stored) = self.scroll.offsets.get(&node) else {
            return Point::ZERO;
        };
        let (max_x, max_y) = self.max_scroll(box_id);
        Point::new(stored.x.clamp(0.0, max_x), stored.y.clamp(0.0, max_y))
    }

    /// Max scroll offsets per axis (0 for non-scroll-container axes).
    fn max_scroll(&self, box_id: BoxId) -> (f32, f32) {
        let b = self.tree().box_(box_id);
        let node = match b.dom_node {
            Some(n) => n,
            None => return (0.0, 0.0),
        };
        let Some((scroll_w, scroll_h)) = self.scroll_size(node) else {
            return (0.0, 0.0);
        };
        let (client_w, client_h) = self.client_size(box_id);
        let max_x = if b.style.overflow.x.is_scroll_container() {
            (scroll_w - client_w).max(0.0)
        } else {
            0.0
        };
        let max_y = if b.style.overflow.y.is_scroll_container() {
            (scroll_h - client_h).max(0.0)
        } else {
            0.0
        };
        (max_x, max_y)
    }

    /// Sets `node`'s scroll offset (clamped). The caller queues a `scroll`
    /// event iff `changed`.
    pub fn set_scroll_offset(&mut self, node: NodeId, x: f32, y: f32) -> ScrollResult {
        let Some(box_id) = self.tree().box_for_node(node) else {
            return ScrollResult {
                x: 0.0,
                y: 0.0,
                changed: false,
            };
        };
        let before = self.box_scroll_offset(box_id);
        let (max_x, max_y) = self.max_scroll(box_id);
        let clamped = Point::new(x.clamp(0.0, max_x), y.clamp(0.0, max_y));
        self.scroll.offsets.insert(node, clamped);
        let changed = clamped != before;
        if changed {
            self.scroll.note_element_changed();
        }
        ScrollResult {
            x: clamped.x,
            y: clamped.y,
            changed,
        }
    }

    /// The clamped viewport (document) scroll offset.
    #[must_use]
    pub fn viewport_scroll(&self) -> Point {
        let (max_x, max_y) = self.max_viewport_scroll();
        Point::new(
            self.scroll.viewport.x.clamp(0.0, max_x),
            self.scroll.viewport.y.clamp(0.0, max_y),
        )
    }

    /// Far edge of the document's scrolling area, in viewport coordinates.
    ///
    /// The root's `scrollable_overflow` is seeded with its *padding* box (CSS
    /// Overflow §3.2), so it alone would drop a border on the root. The document
    /// scrolling area is bounded by the root's border box, hence the union here.
    #[must_use]
    pub fn document_content_extent(&self) -> (f32, f32) {
        let Some(root) = self.tree().root() else {
            return (0.0, 0.0);
        };
        let root_box = self.tree().box_(root);
        let layout = &root_box.final_layout;
        let border_box = Rect::from_xywh(0.0, 0.0, layout.size.width, layout.size.height);
        let extent = root_box.scrollable_overflow.union(&border_box);
        (
            extent.max_x() + layout.location.x,
            extent.max_y() + layout.location.y,
        )
    }

    fn max_viewport_scroll(&self) -> (f32, f32) {
        let viewport = self.viewport();
        let Some(root) = self.tree().root() else {
            return (0.0, 0.0);
        };
        let root_box = self.tree().box_(root);
        // The overflow pass unions every descendant into the root's scrollable
        // overflow regardless of the root's own `overflow` (clipping only trims
        // what a box contributes to its *parent*, and the root has none). So a
        // non-scrollable `overflow` on the root has to be honored here, or
        // `<html style="overflow:hidden">` would still scroll the document.
        // `scroll`/`auto` keep scrolling; only `hidden`/`clip` pin the document.
        let scrollable = |overflow: taffy::Overflow| {
            !matches!(overflow, taffy::Overflow::Hidden | taffy::Overflow::Clip)
        };
        let (content_x, content_y) = self.document_content_extent();
        // A non-scrollable axis reports the viewport extent, so its max scroll
        // clamps to zero. (`scrollWidth`/`scrollHeight` still see the real
        // overflow — that is a separate query.)
        let extent_x = if scrollable(root_box.style.overflow.x) {
            content_x
        } else {
            viewport.width
        };
        let extent_y = if scrollable(root_box.style.overflow.y) {
            content_y
        } else {
            viewport.height
        };
        (
            (extent_x - viewport.width).max(0.0),
            (extent_y - viewport.height).max(0.0),
        )
    }

    /// Sets the viewport scroll (clamped); `window.scrollTo` and
    /// `documentElement.scrollTop` route here.
    pub fn set_viewport_scroll(&mut self, x: f32, y: f32) -> ScrollResult {
        let before = self.viewport_scroll();
        let (max_x, max_y) = self.max_viewport_scroll();
        let clamped = Point::new(x.clamp(0.0, max_x), y.clamp(0.0, max_y));
        self.scroll.viewport = clamped;
        let changed = clamped != before;
        if changed {
            self.scroll.note_document_changed();
        }
        ScrollResult {
            x: clamped.x,
            y: clamped.y,
            changed,
        }
    }

    /// The computed `position` of `node`'s box, if it generates one.
    #[must_use]
    pub fn node_position(&self, node: NodeId) -> Option<Position> {
        self.tree()
            .box_for_node(node)
            .map(|b| self.tree().box_(b).position)
    }

    /// Whether `node`'s box has the initial `overflow: visible` on *both*
    /// axes — `None` when `node` has no box. Used by `document.scrollingElement`
    /// (CSSOM-View "potentially scrollable"): quirks mode's root↔body overflow
    /// propagation is not modeled anywhere in the cascade here, so the
    /// bindings layer reads each element's own (unpropagated) overflow via
    /// this and applies the propagation rule itself.
    #[must_use]
    pub fn overflow_is_visible(&self, node: NodeId) -> Option<bool> {
        let box_id = self.tree().box_for_node(node)?;
        let b = self.tree().box_(box_id);
        Some(
            b.style.overflow.x == taffy::Overflow::Visible
                && b.style.overflow.y == taffy::Overflow::Visible,
        )
    }

    // === scrollParent ===

    /// Whether `box_id` is a `position: fixed` box whose containing block
    /// (already resolved onto `box.parent` by `hoist_out_of_flow`) fell
    /// straight through to the root with no real ancestor establishing one —
    /// i.e. it is fixed to the viewport, which nothing DOM-observable
    /// scrolls.
    fn stuck_to_viewport(&self, box_id: BoxId, root: BoxId) -> bool {
        self.tree().box_(box_id).position == Position::Fixed
            && self.tree().box_(box_id).parent == Some(root)
    }

    /// `Element.scrollParent()` (CSSOM-View, draft): the nearest ancestor
    /// along the containing-block chain that is a scroll container.
    ///
    /// The walk itself needs no bespoke containing-block logic: out-of-flow
    /// boxes (`position: absolute`/`fixed`) are already re-parented onto
    /// their resolved containing block by `hoist_out_of_flow` before the
    /// first layout (`positioning::containing_block`), so the plain
    /// box-tree `parent` chain used below *is* the containing-block chain,
    /// for in-flow and out-of-flow boxes alike — including the flat tree,
    /// since the box tree is built from `flat_tree_children`.
    #[must_use]
    pub fn scroll_parent(&self, dom: &DomTree, element: NodeId) -> ScrollParent {
        let Some(root) = self.tree().root() else {
            return ScrollParent::None;
        };
        let Some(box_id) = self.tree().box_for_node(element) else {
            return ScrollParent::None;
        };
        if box_id == root || self.stuck_to_viewport(box_id, root) {
            return ScrollParent::None;
        }

        let mut ancestor = self.tree().box_(box_id).parent;
        loop {
            let Some(current) = ancestor else {
                return ScrollParent::None;
            };
            if current == root {
                return ScrollParent::DocumentScrollingElement;
            }
            let b = self.tree().box_(current);
            let is_container = b.style.overflow.x.is_scroll_container()
                || b.style.overflow.y.is_scroll_container();
            if is_container
                && let Some(node) = b.dom_node
                && is_shadow_reachable(dom, node, element)
            {
                return ScrollParent::Element(node);
            }
            if self.stuck_to_viewport(current, root) {
                return ScrollParent::None;
            }
            ancestor = self.tree().box_(current).parent;
        }
    }
}

/// Whether `candidate` is reachable from `element` by walking DOM parents and
/// shadow-host boundaries only, never through slot assignment — i.e. lies on
/// `element`'s own tree or one of its ancestor shadow trees (the DOM
/// "shadow-including ancestor" relation). `scrollParent`'s containing-block
/// walk runs over the *flat* tree, which can step from light-DOM content into
/// a shadow tree via slot assignment; a candidate only reachable that way is
/// invisible to `element` regardless of the shadow root's mode, matching WPT
/// `scrollParent-shadow-tree.html` (both `open` and `closed` components hide
/// their internal scroll container from slotted light-DOM content there).
fn is_shadow_reachable(dom: &DomTree, candidate: NodeId, element: NodeId) -> bool {
    let mut current = element;
    loop {
        if current == candidate {
            return true;
        }
        if let Some(parent) = dom.ancestors(current).next() {
            current = parent;
        } else if let Some(host) = dom.shadow_host(current) {
            current = host;
        } else {
            return false;
        }
    }
}
