//! The paint walk: turns a laid-out box tree into a [`DisplayList`] by walking
//! it in stacking-context order (ADR-0007 D5, a pragmatic subset of CSS 2.1
//! Appendix E).
//!
//! Per box: `opacity: 0` skips the subtree; `0 < opacity < 1` wraps it in a
//! [`DisplayItem::PushLayer`]; `visibility: hidden` skips the box's own paint
//! but descends into children; `overflow != visible` wraps the box's content
//! (steps 3–7) in a padding-box clip. Children paint back-to-front, ordered by
//! `(stacking bucket, z-index, tree order)`.

use oxidepage_base::{Point, Rect, Size, Transform2D};
use oxidepage_dom::{DomTree, NodeId};
use oxidepage_layout::{BoxId, LayoutEngine, PseudoBox, ReplacedContent};
use servo_arc::Arc as ServoArc;
use style::computed_values::object_fit::T as ObjectFit;
use style::computed_values::position::T as Position;
use style::computed_values::visibility::T as Visibility;
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement;
use style::values::computed::Length;

use crate::convert;
use crate::display_list::{BorderRadii, Brush, Color, DisplayItem, DisplayList, ResourceTable};
use crate::text::InlinePhase;

/// Hard cap on paint-walk nesting depth. The walk (`paint_box` →
/// `paint_box_at` → `paint_box`) recurses once per box-tree level, so a
/// pathologically deep tree (e.g. thousands of nested elements) would overflow
/// the stack. Past this depth the walk stops descending: the (already
/// off-screen-scale) deepest subtrees are simply not painted, which degrades
/// gracefully instead of crashing. Well beyond any realistic document nesting.
const MAX_PAINT_DEPTH: usize = 256;

/// Knobs that change *what* is painted, as opposed to where it lands.
///
/// `print_background` is a **build** option and not a PDF one on purpose: by
/// export time an element background is an ordinary [`DisplayItem::Fill`],
/// indistinguishable from any other fill, so the only place that can drop it is
/// the walk that knows what it is (ADR-0026).
#[derive(Clone, Copy, Debug)]
pub struct PaintOptions {
    /// Paint element backgrounds — colors, gradients and background images —
    /// and the canvas background propagated from `<html>`/`<body>`. The opaque
    /// white base fill and replaced content (`<img>`) are unaffected.
    ///
    /// **Defaults to `true`, unlike Chrome's `printBackground`**, so
    /// `render -o page.pdf` keeps meaning "the page as it looks" (ADR-0026).
    pub print_background: bool,
}

impl Default for PaintOptions {
    fn default() -> Self {
        Self {
            print_background: true,
        }
    }
}

/// Builds the display list for the current layout of `dom`/`engine`.
///
/// **There is one entry point, and it covers the whole document.** There used
/// to be a `build_display_list_full` beside it for the PDF and full-page
/// callers; the list became scroll-independent and the two bodies became
/// identical, leaving a name that promised a distinction the code did not make.
/// A viewport render and a full-page one differ only in what the *rasterizer*
/// is told to cover (`raster_skia::render_scrolled` vs `render_full_page`),
/// never in what is built here.
///
/// The list is built *unscrolled*: document content is placed at its document
/// position and the document (viewport) scroll is applied later, by the
/// rasterizer, so one cached list serves every scroll position. A
/// `position: fixed` subtree is wrapped in a [`DisplayItem::PushViewportAnchor`]
/// / [`DisplayItem::PopViewportAnchor`] pair so the rasterizer leaves it pinned
/// to the viewport while document content scrolls under it. Element
/// (`overflow`) scroll offsets *are* baked into item origins here.
///
/// The paint walk reads computed styles through `dom.primary_style` /
/// `dom.pseudo_style`, which do not need an active-tree scope (ADR-0007 D2).
/// It borrows the tree immutably and never calls back into JS.
#[must_use]
pub fn build_display_list(
    dom: &DomTree,
    engine: &LayoutEngine,
    options: &PaintOptions,
) -> DisplayList {
    let viewport = engine.viewport();
    let viewport_size = Size::new(viewport.width, viewport.height);
    let tree = engine.tree();

    let Some(root) = tree.root() else {
        return DisplayList::empty(viewport_size);
    };

    let (extent_x, extent_y) = engine.document_content_extent();
    let content_size = Size::new(extent_x.max(viewport.width), extent_y.max(viewport.height));

    let mut builder = Builder {
        dom,
        engine,
        options: *options,
        items: Vec::new(),
        resources: ResourceTable::default(),
        suppressed_bg: None,
        depth: 0,
        origins: Vec::new(),
        positioned_inline: Vec::new(),
    };
    builder.compute_origins(root);
    builder.compute_positioned_inline(root);

    // Opaque-white base plus the propagated canvas background (ADR-0007 D7).
    builder.paint_canvas(root, content_size, viewport_size);

    builder.paint_box(root);

    DisplayList {
        viewport: viewport_size,
        content_size,
        items: builder.items,
        resources: builder.resources,
    }
}

pub(crate) struct Builder<'a> {
    dom: &'a DomTree,
    engine: &'a LayoutEngine,
    options: PaintOptions,
    items: Vec<DisplayItem>,
    resources: ResourceTable,
    /// The node whose background was propagated to the canvas; it must not
    /// paint its own background box again.
    suppressed_bg: Option<NodeId>,
    /// Current paint-walk recursion depth (see [`MAX_PAINT_DEPTH`]).
    depth: usize,
    /// Every box's border-box origin in paint coordinates, indexed by `BoxId`.
    ///
    /// Origins are accumulated down the *layout* tree (a box's location is
    /// relative to its containing block), while the paint walk descends the
    /// *DOM* tree — the two differ for a hoisted out-of-flow box, which is why
    /// the painter looks its origin up here instead of accumulating it.
    origins: Vec<Point>,
    /// Per box: whether its in-flow subtree emits any positioned-inline
    /// content (text or an atomic inline inside a positioned inline element).
    /// The positioned-inline paint pass ([`Self::paint_positioned_inline_text`])
    /// is skipped wherever this is false — the common case, where it would
    /// otherwise re-walk the IFCs for nothing (see
    /// [`Self::compute_positioned_inline`]).
    positioned_inline: Vec<bool>,
}

impl<'a> Builder<'a> {
    /// The computed style for a box: its principal style, or the matching
    /// pseudo-element style; `None` for anonymous boxes.
    fn style_for(
        &self,
        node: Option<NodeId>,
        pseudo: Option<PseudoBox>,
    ) -> Option<ServoArc<ComputedValues>> {
        let node = node?;
        match pseudo {
            None => self.dom.primary_style(node),
            Some(PseudoBox::Before) => self.dom.pseudo_style(node, &PseudoElement::Before),
            Some(PseudoBox::After) => self.dom.pseudo_style(node, &PseudoElement::After),
            // A list marker takes the item's inherited style (`::marker` is not
            // a styleable pseudo-element here; see `layout::marker`), so the
            // bullet paints in the item's colour and font.
            Some(PseudoBox::Marker) => self.dom.primary_style(node),
        }
    }

    /// Scroll offset applied to a box's children (0 for boxes without a node).
    fn child_scroll(&self, node: Option<NodeId>) -> Point {
        node.map_or(Point::ZERO, |n| self.engine.scroll_offset(n))
    }

    /// Paints the opaque-white base and the propagated canvas background over
    /// the whole canvas area (ADR-0007 D7: html → body propagation).
    fn paint_canvas(&mut self, root: BoxId, content_size: Size, viewport_size: Size) {
        let canvas = Rect::from_xywh(
            0.0,
            0.0,
            content_size.width.max(viewport_size.width),
            content_size.height.max(viewport_size.height),
        );
        self.items.push(DisplayItem::Fill {
            rect: canvas,
            radii: BorderRadii::ZERO,
            brush: Brush::Solid(Color::WHITE),
        });
        // The white base stays even without backgrounds — a PDF page is opaque
        // paper — but the page's own canvas color is a background like any
        // other.
        if !self.options.print_background {
            return;
        }

        // The root box's DOM node is <html>.
        let html = self.engine.tree().box_(root).dom_node;
        let html_bg = html
            .and_then(|n| self.dom.primary_style(n))
            .map(|s| convert::background_color(&s));

        let (source_node, source_color) = match html_bg {
            Some(c) if !c.is_transparent() => (html, Some(c)),
            _ => {
                // Fall back to <body>'s background.
                let body = html.and_then(|h| self.body_of(h));
                let body_bg = body
                    .and_then(|n| self.dom.primary_style(n))
                    .map(|s| convert::background_color(&s));
                match body_bg {
                    Some(c) if !c.is_transparent() => (body, Some(c)),
                    _ => (None, None),
                }
            }
        };

        if let (Some(node), Some(color)) = (source_node, source_color) {
            self.items.push(DisplayItem::Fill {
                rect: canvas,
                radii: BorderRadii::ZERO,
                brush: Brush::Solid(color),
            });
            self.suppressed_bg = Some(node);
        }
    }

    /// The first `<body>` element child of `html`.
    fn body_of(&self, html: NodeId) -> Option<NodeId> {
        self.dom.children(html).find(|&c| {
            self.dom
                .node(c)
                .as_element()
                .is_some_and(|el| el.is_html_element() && &*el.name.local == "body")
        })
    }

    /// Fills [`Self::origins`] by walking the layout tree from `root`: each
    /// box's origin is its containing block's, less that block's element scroll
    /// offset, plus its own location. The document (viewport) scroll is *not*
    /// applied here — the list is built unscrolled and the rasterizer offsets
    /// document content by the live scroll, leaving `position: fixed` subtrees
    /// (which it identifies from the viewport-anchor markers) pinned.
    fn compute_origins(&mut self, root: BoxId) {
        let tree = self.engine.tree();
        self.origins = vec![Point::ZERO; tree.box_count()];

        let root_box = tree.box_(root);
        self.origins[root.index()] = Point::new(
            root_box.final_layout.location.x,
            root_box.final_layout.location.y,
        );

        let mut stack = vec![root];
        while let Some(parent) = stack.pop() {
            let origin = self.origins[parent.index()];
            let scroll = self.child_scroll(tree.box_(parent).dom_node);
            for &child in &tree.box_(parent).children {
                let cb = tree.box_(child);
                let location = cb.final_layout.location;
                self.origins[child.index()] = Point::new(
                    origin.x - scroll.x + location.x,
                    origin.y - scroll.y + location.y,
                );
                stack.push(child);
            }
        }
    }

    /// Fills [`Self::positioned_inline`]: a box is flagged when it, or any of
    /// its in-flow (static) descendants, emits positioned-inline content —
    /// exactly the subtree [`Self::paint_positioned_inline_text`] would walk.
    /// Computed once per build so the (unconditional, per positioned container)
    /// second walk can be skipped wherever there is nothing to paint.
    fn compute_positioned_inline(&mut self, root: BoxId) {
        let tree = self.engine.tree();
        let n = tree.box_count();
        // Own flag per box: does its own IFC emit positioned-inline content?
        let mut flags: Vec<bool> = (0..n)
            .map(|i| {
                let id = BoxId::from(taffy::NodeId::from(i as u64));
                crate::text::ifc_has_positioned_inline(self.engine, self.dom, id)
            })
            .collect();

        // Propagate up static-child edges. A pre-order push sequence, reversed,
        // visits every descendant before its ancestor (a parent always precedes
        // its descendants in pre-order), so one bottom-up pass suffices.
        let mut order = Vec::with_capacity(n);
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            order.push(id);
            for &child in &tree.box_(id).children {
                stack.push(child);
            }
        }
        for &id in order.iter().rev() {
            if flags[id.index()] {
                continue;
            }
            // A multicol root is a barrier: its flow's positioned-inline text is
            // emitted by `paint_box_inner`, once per column, inside the column
            // clip + transform. Propagating the flag past it would have the
            // nearest positioned *ancestor* walk down into the flow and paint that
            // text again, unclipped and untranslated, straight down the page.
            if tree.box_(id).multicol.is_some() {
                continue;
            }
            for &child in &tree.box_(id).children {
                let cb = tree.box_(child);
                if cb.position == Position::Static && flags[child.index()] {
                    flags[id.index()] = true;
                    break;
                }
            }
        }
        self.positioned_inline = flags;
    }

    /// Paints `box_id` and its subtree. Wraps [`Self::paint_box_inner`] with a
    /// hard recursion-depth cap ([`MAX_PAINT_DEPTH`]) so a pathologically deep
    /// box tree cannot overflow the stack.
    fn paint_box(&mut self, box_id: BoxId) {
        if self.depth >= MAX_PAINT_DEPTH {
            return;
        }
        self.depth += 1;
        self.paint_box_inner(box_id);
        self.depth -= 1;
    }

    fn paint_box_inner(&mut self, box_id: BoxId) {
        let abs_origin = self.origins[box_id.index()];
        let tree = self.engine.tree();
        let b = tree.box_(box_id);
        let node = b.dom_node;
        let pseudo = b.pseudo;
        let style = self.style_for(node, pseudo);

        let opacity = style.as_deref().map_or(1.0, convert::opacity);
        if opacity <= 0.0 {
            return;
        }
        let visible = style
            .as_deref()
            .is_none_or(|s| s.get_inherited_box().clone_visibility() == Visibility::Visible);

        // A `transform` is carried on the same layer item as `opacity`: it maps
        // this box's whole subtree (which the builder lays out untransformed) at
        // paint time. Both a transformed and a translucent box are therefore
        // layered, and a box that is both needs only one item.
        let size = b.final_layout.size;
        let border_box = Rect::from_xywh(abs_origin.x, abs_origin.y, size.width, size.height);
        let transform = style
            .as_deref()
            .and_then(|s| oxidepage_layout::transform::resolve(s, border_box))
            .unwrap_or(Transform2D::IDENTITY);

        // A `position: fixed` box is pinned to the viewport: wrap its whole
        // subtree so the rasterizer suppresses the document scroll for it. The
        // marker sits outside the opacity/transform layer — the box's own
        // transform still maps its (unscrolled, viewport-space) content.
        let viewport_anchored = b.position == Position::Fixed;
        if viewport_anchored {
            self.items.push(DisplayItem::PushViewportAnchor);
        }

        let layered = opacity < 1.0 || transform != Transform2D::IDENTITY;
        if layered {
            self.items
                .push(DisplayItem::PushLayer { opacity, transform });
        }

        // Step 1: the box's own background + border (never clipped).
        if visible && let Some(s) = style.as_deref() {
            self.paint_decorations(box_id, abs_origin, s, node);
        }

        // Overflow clip wraps the box's content (steps 3–7). A single-axis clip
        // — `overflow-x: hidden; overflow-y: visible`, or the `<body>` after
        // viewport overflow propagation leaves it `overflow-x: hidden` — must
        // leave the visible axis unbounded, so the padding-box clip is stretched
        // to the box's scrollable overflow on any axis that does not itself clip.
        // Clipping both axes to the padding box (the old behaviour) truncated the
        // document to one viewport whenever the body scrolled with a clipped x.
        let clip_x = b.style.overflow.x != taffy::Overflow::Visible;
        let clip_y = b.style.overflow.y != taffy::Overflow::Visible;
        if clip_x || clip_y {
            let (mut rect, radii) = self.padding_box_clip(box_id, abs_origin, style.as_deref());
            let ov = b.scrollable_overflow;
            if !clip_x {
                let left = rect.min_x().min(abs_origin.x + ov.min_x());
                let right = rect.max_x().max(abs_origin.x + ov.max_x());
                rect.origin.x = left;
                rect.size.width = right - left;
            }
            if !clip_y {
                let top = rect.min_y().min(abs_origin.y + ov.min_y());
                let bottom = rect.max_y().max(abs_origin.y + ov.max_y());
                rect.origin.y = top;
                rect.size.height = bottom - top;
            }
            self.items.push(DisplayItem::PushClip { rect, radii });
        }

        // Step 5: the box's own inline content (glyph runs + atomic inline
        // boxes, positioned by the IFC). Atomic inline children are painted
        // here, so they are skipped in the block-flow loop below.
        let inline_children = self.inline_box_children(box_id);
        if visible {
            self.paint_text(box_id, abs_origin, style.as_deref(), InlinePhase::InFlow);
            self.paint_replaced(box_id, abs_origin, style.as_deref());
        }

        // Step 5b: multi-column (ADR-0016). The single anonymous flow child is
        // not painted by the child loop below — it is painted once *per column*,
        // clipped to the column box and translated so the column's slice of the
        // continuous flow lands at the top of the column. The flow's items
        // already carry flow-absolute origins (`compute_origins` walks the layout
        // tree), so each column is a pure clip + translate view: text flows across
        // columns and a background straddling a break is sliced correctly, with no
        // change to `paint_ifc`.
        let multicol = self.multicol_columns(box_id);
        if visible && let Some((flow, width, origin, columns)) = &multicol {
            for column in columns {
                // The clip height is the column's *slice*, never the used column
                // height: when the fill stops at a break opportunity above the
                // column bottom (the normal case), the strip below it holds the
                // *next* column's content, which would otherwise show through.
                let height = column.end - column.start;
                if height <= 0.0 || *width <= 0.0 {
                    continue;
                }

                // The clip goes *outside* the layer on purpose: both backends map
                // a clip path through the current transform, so a clip pushed
                // inside the layer would be translated along with the content and
                // would not clip at all.
                self.items.push(DisplayItem::PushClip {
                    rect: Rect::from_xywh(origin.x + column.x, origin.y, *width, height),
                    radii: BorderRadii::ZERO,
                });
                let transform = Transform2D::translation(column.x, -column.start);
                let layered = transform != Transform2D::IDENTITY;
                if layered {
                    self.items.push(DisplayItem::PushLayer {
                        opacity: 1.0,
                        transform,
                    });
                }

                self.paint_box(*flow);
                // Text inside a `position: relative` inline paints in the
                // Positioned phase, which is normally emitted from the nearest
                // positioned ancestor — outside this clip and transform. Emit it
                // here instead: `compute_positioned_inline` makes a multicol root
                // a barrier so no ancestor tries.
                if self.positioned_inline[flow.index()] {
                    self.paint_positioned_inline_text(*flow);
                }

                if layered {
                    self.items.push(DisplayItem::PopLayer);
                }
                self.items.push(DisplayItem::PopClip);
            }
        }

        // Steps 2/3/6/7: children in back-to-front stacking order.
        for child in self.ordered_children(box_id) {
            if inline_children.contains(&child) {
                continue;
            }
            if multicol.as_ref().is_some_and(|(flow, ..)| *flow == child) {
                continue;
            }
            self.paint_box(child);
        }

        // Steps 6–8: text inside a positioned inline element paints above the
        // positioned descendants, not with the in-flow inline content. It is
        // emitted from the nearest positioned ancestor — the stacking container
        // it actually belongs to — because the IFC holding it can sit any number
        // of in-flow boxes further down: an `<h2>` whose text is wrapped in a
        // relative `<span>` puts that text in an *anonymous block*, and painting
        // it from there would still leave it under the `<h2>`'s absolutely
        // positioned `::before`. Emitted inside this box's overflow clip.
        let positioned_container = self.engine.tree().box_(box_id).position != Position::Static
            || Some(box_id) == self.engine.tree().root();
        if positioned_container && self.positioned_inline[box_id.index()] {
            self.paint_positioned_inline_text(box_id);
        }

        if clip_x || clip_y {
            self.items.push(DisplayItem::PopClip);
        }
        if layered {
            self.items.push(DisplayItem::PopLayer);
        }
        if viewport_anchored {
            self.items.push(DisplayItem::PopViewportAnchor);
        }
    }

    /// The per-column view parameters of a multi-column container:
    /// `(flow box, used column width, the flow's absolute origin, the columns)`.
    /// `None` for every other box.
    ///
    /// The flow box sits at the container's content-box origin and a column's `x`
    /// is measured from there, so its entry in [`Self::origins`] — already
    /// rounded, already element-scrolled — is exactly the origin the column clips
    /// and translates are relative to.
    fn multicol_columns(
        &self,
        box_id: BoxId,
    ) -> Option<(BoxId, f32, Point, Vec<oxidepage_layout::ColumnRange>)> {
        let mc = self.engine.tree().box_(box_id).multicol.as_deref()?;
        let flow = mc.flow();
        Some((
            flow,
            mc.used_width(),
            self.origins[flow.index()],
            mc.columns().to_vec(),
        ))
    }

    /// Emits the background fill and border for a styled box.
    fn paint_decorations(
        &mut self,
        box_id: BoxId,
        abs_origin: Point,
        style: &ComputedValues,
        node: Option<NodeId>,
    ) {
        let b = self.engine.tree().box_(box_id);
        let size = b.final_layout.size;
        let border_box = Rect::from_xywh(abs_origin.x, abs_origin.y, size.width, size.height);
        let radii = convert::border_radii(style, Size::new(size.width, size.height));
        let box_edges = crate::background::Edges {
            border: b.final_layout.border,
        };

        // Background: color plus layers (suppressed color on the canvas node).
        if self.options.print_background {
            crate::background::paint(
                self,
                border_box,
                box_edges,
                radii,
                style,
                node != self.suppressed_bg,
            );
        }

        // Borders.
        let b = self.engine.tree().box_(box_id);
        let edges = convert::border_edges(style, b.final_layout.border);
        if edges.iter().any(super::display_list::BorderEdge::paints) {
            self.items.push(DisplayItem::Border {
                rect: border_box,
                radii,
                edges,
            });
        }
    }

    /// Paints a replaced element's content: a decoded `<img>` over its content
    /// box (stretched), or a gray placeholder when the image is broken/missing
    /// but the box has a size (ADR-0007 D7, WP-L).
    fn paint_replaced(&mut self, box_id: BoxId, abs_origin: Point, style: Option<&ComputedValues>) {
        let (content, data) = {
            let b = self.engine.tree().box_(box_id);
            let Some(ReplacedContent::Image(ctx)) = &b.replaced else {
                return;
            };
            let bl = b.final_layout.border;
            let pad = b.final_layout.padding;
            let size = b.final_layout.size;
            let content = Rect::from_xywh(
                abs_origin.x + bl.left + pad.left,
                abs_origin.y + bl.top + pad.top,
                (size.width - bl.left - bl.right - pad.left - pad.right).max(0.0),
                (size.height - bl.top - bl.bottom - pad.top - pad.bottom).max(0.0),
            );
            (content, ctx.data.clone())
        };
        if content.is_empty() {
            return;
        }
        match data {
            Some(image) => {
                // `object-fit` decides how the image's intrinsic box maps onto the
                // content box; `object-position` places it. Without this the image
                // is stretched to the box (the `fill` behaviour), which distorts
                // any `cover`/`contain` icon or photo (ADR-0007). `cover`/`none`
                // can overflow, so the draw is clipped to the content box.
                let dst = object_fit_dst(content, image.width, image.height, style);
                const EPS: f32 = 0.5;
                let clipped = dst.min_x() < content.min_x() - EPS
                    || dst.min_y() < content.min_y() - EPS
                    || dst.max_x() > content.max_x() + EPS
                    || dst.max_y() > content.max_y() + EPS;
                if clipped {
                    self.items.push(DisplayItem::PushClip {
                        rect: content,
                        radii: BorderRadii::ZERO,
                    });
                }
                let id = image.id;
                self.resources.add_image(image);
                self.items.push(DisplayItem::Image {
                    dst,
                    image: id,
                    tile: crate::display_list::TileMode::Stretch,
                    radii: BorderRadii::ZERO,
                });
                if clipped {
                    self.items.push(DisplayItem::PopClip);
                }
            }
            None => {
                // Broken / not-yet-loaded image with a sized box: placeholder.
                self.items.push(DisplayItem::Fill {
                    rect: content,
                    radii: BorderRadii::ZERO,
                    brush: Brush::Solid(Color::rgb(192, 192, 192)),
                });
            }
        }
    }

    /// The padding-box rect and inner (border-adjusted) radii used to clip an
    /// `overflow != visible` box.
    fn padding_box_clip(
        &self,
        box_id: BoxId,
        abs_origin: Point,
        style: Option<&ComputedValues>,
    ) -> (Rect, BorderRadii) {
        let b = self.engine.tree().box_(box_id);
        let bl = b.final_layout.border;
        let size = b.final_layout.size;
        let pad_origin = Point::new(abs_origin.x + bl.left, abs_origin.y + bl.top);
        let pad_size = Size::new(
            (size.width - bl.left - bl.right).max(0.0),
            (size.height - bl.top - bl.bottom).max(0.0),
        );
        let outer = style.map_or(BorderRadii::ZERO, |s| {
            convert::border_radii(s, Size::new(size.width, size.height))
        });
        let inner = BorderRadii {
            top_left: Size::new(
                (outer.top_left.width - bl.left).max(0.0),
                (outer.top_left.height - bl.top).max(0.0),
            ),
            top_right: Size::new(
                (outer.top_right.width - bl.right).max(0.0),
                (outer.top_right.height - bl.top).max(0.0),
            ),
            bottom_right: Size::new(
                (outer.bottom_right.width - bl.right).max(0.0),
                (outer.bottom_right.height - bl.bottom).max(0.0),
            ),
            bottom_left: Size::new(
                (outer.bottom_left.width - bl.left).max(0.0),
                (outer.bottom_left.height - bl.bottom).max(0.0),
            ),
        }
        .clamped_to(pad_size);
        (Rect::new(pad_origin, pad_size), inner)
    }

    /// Children ordered back-to-front: positioned `z < 0`, then in-flow, then
    /// positioned `z: auto/0`, then positioned `z > 0`; ties break by z-index
    /// then tree order (mirrors the hit-test order in `layout::geometry`).
    ///
    /// Out-of-flow boxes hoisted *out of* this one are painted here — CSS
    /// stacks a positioned box among its DOM siblings, not among the children
    /// of the containing block it is laid out against (mgid.com's off-canvas
    /// menu is `z-index: 100` inside a header whose own controls are `101`).
    /// Tree order is the `BoxId`, which is allocated in DOM order, so a hoisted
    /// box still ties against its siblings where it belongs.
    fn ordered_children(&self, box_id: BoxId) -> Vec<BoxId> {
        let tree = self.engine.tree();
        let parent = tree.box_(box_id);
        let mut ordered: Vec<(u8, i32, usize, BoxId)> = parent
            .children
            .iter()
            // A hoisted box is laid out here but painted by the parent it was
            // built under (which lists it in `hoisted_children`).
            .filter(|&&child| tree.box_(child).static_parent.is_none())
            .chain(parent.hoisted_children.iter())
            .map(|&child| {
                let cb = tree.box_(child);
                let positioned = cb.position != Position::Static;
                let bucket = if positioned && cb.z_index < 0 {
                    0
                } else if !positioned {
                    1
                } else if cb.z_index == 0 {
                    2
                } else {
                    3
                };
                (bucket, cb.z_index, child.index(), child)
            })
            .collect();
        ordered.sort_by_key(|e| (e.0, e.1, e.2));
        ordered.into_iter().map(|e| e.3).collect()
    }

    /// Paints the [`InlinePhase::Positioned`] text of `box_id` and of every
    /// in-flow box below it, stopping at boxes that are positioned themselves
    /// (they emit their own, in their own stacking order).
    fn paint_positioned_inline_text(&mut self, box_id: BoxId) {
        if self.depth >= MAX_PAINT_DEPTH {
            return;
        }
        self.depth += 1;

        let tree = self.engine.tree();
        let b = tree.box_(box_id);
        let (node, pseudo) = (b.dom_node, b.pseudo);
        let children = b.children.clone();
        let style = self.style_for(node, pseudo);
        let visible = style
            .as_deref()
            .is_none_or(|s| s.get_inherited_box().clone_visibility() == Visibility::Visible);

        if visible {
            let origin = self.origins[box_id.index()];
            self.paint_text(box_id, origin, style.as_deref(), InlinePhase::Positioned);
        }
        for child in children {
            let (is_static, clips, child_node, child_pseudo) = {
                let cb = self.engine.tree().box_(child);
                let clips = cb.style.overflow.x != taffy::Overflow::Visible
                    || cb.style.overflow.y != taffy::Overflow::Visible;
                (
                    cb.position == Position::Static,
                    clips,
                    cb.dom_node,
                    cb.pseudo,
                )
            };
            // Only in-flow descendants belong to this stacking context, and
            // only those whose subtree carries positioned-inline content are
            // worth walking (see `compute_positioned_inline`).
            if !is_static || !self.positioned_inline[child.index()] {
                continue;
            }
            // Re-establish the child's overflow clip around its escaped
            // positioned-inline text: the main walk's clip pair is long popped
            // by the time this pass runs, so without this the text paints
            // unclipped through an intermediate `overflow: hidden` box.
            if clips {
                let origin = self.origins[child.index()];
                let child_style = self.style_for(child_node, child_pseudo);
                let (rect, radii) = self.padding_box_clip(child, origin, child_style.as_deref());
                self.items.push(DisplayItem::PushClip { rect, radii });
            }
            self.paint_positioned_inline_text(child);
            if clips {
                self.items.push(DisplayItem::PopClip);
            }
        }

        self.depth -= 1;
    }

    /// Paints one phase of a box's inline formatting context (glyph runs,
    /// decorations); see [`crate::text::InlinePhase`].
    fn paint_text(
        &mut self,
        box_id: BoxId,
        abs_origin: Point,
        style: Option<&ComputedValues>,
        phase: crate::text::InlinePhase,
    ) {
        crate::text::paint_ifc(self, box_id, abs_origin, style, phase);
    }

    /// The atomic-inline child boxes placed by this box's IFC (empty when it
    /// has none); those are painted by [`crate::text::paint_ifc`], not the
    /// block-flow child loop.
    fn inline_box_children(&self, box_id: BoxId) -> std::collections::HashSet<BoxId> {
        let mut set = std::collections::HashSet::new();
        let b = self.engine.tree().box_(box_id);
        // Atomic inline boxes are in-flow child boxes placed by the IFC; a box
        // with no child boxes (plain text, or non-atomic inline spans) has
        // none, so skip the whole line/item walk — the common case, avoiding a
        // second O(items) traversal (paint_ifc walks the same lines) per block.
        if b.children.is_empty() {
            return set;
        }
        if let Some(ifc) = b.ifc.as_ref() {
            for line in ifc.layout.lines() {
                for item in line.items() {
                    if let parley::PositionedLayoutItem::InlineBox(ibox) = item {
                        set.insert(BoxId::from(taffy::NodeId::from(ibox.id)));
                    }
                }
            }
        }
        set
    }

    /// Pushes a display item (used by `text.rs`).
    pub(crate) fn push(&mut self, item: DisplayItem) {
        self.items.push(item);
    }

    /// Paints an atomic-inline child box (used by `text.rs`). The IFC already
    /// wrote the box's placement into its layout, so its origin — like every
    /// other box's — comes from [`Self::origins`].
    pub(crate) fn paint_inline_box(&mut self, box_id: BoxId) {
        self.paint_box(box_id);
    }

    /// Access to the layout engine (borrowed for the builder's whole life, so
    /// it can be read while the builder is mutated; used by `text.rs`).
    pub(crate) fn engine(&self) -> &'a LayoutEngine {
        self.engine
    }

    /// Access to the DOM (see [`Self::engine`]).
    pub(crate) fn dom(&self) -> &'a DomTree {
        self.dom
    }

    /// Records a font resource, deduplicated by id (used by `text.rs`).
    pub(crate) fn add_font(&mut self, resource: crate::display_list::FontResource) {
        self.resources.add_font(resource);
    }

    /// Records a decoded image, deduplicated by id (used by `background.rs`).
    pub(crate) fn add_image(&mut self, image: std::sync::Arc<crate::display_list::DecodedImage>) {
        self.resources.add_image(image);
    }
}

pub(crate) use Builder as PaintBuilder;

/// The concrete size a replaced image's pixels occupy under `object-fit`
/// (CSS Images 3 §5.5), given the content box (`cw`×`ch`) and the image's
/// intrinsic size (`iw`×`ih`, both assumed positive). The aspect ratio is
/// preserved for every value except `fill`.
fn object_fit_size(fit: ObjectFit, cw: f32, ch: f32, iw: f32, ih: f32) -> (f32, f32) {
    match fit {
        ObjectFit::Fill => (cw, ch),
        ObjectFit::Contain => {
            let s = (cw / iw).min(ch / ih);
            (iw * s, ih * s)
        }
        ObjectFit::Cover => {
            let s = (cw / iw).max(ch / ih);
            (iw * s, ih * s)
        }
        ObjectFit::None => (iw, ih),
        // `none` or `contain`, whichever yields the smaller concrete size.
        ObjectFit::ScaleDown => {
            let s = (cw / iw).min(ch / ih).min(1.0);
            (iw * s, ih * s)
        }
    }
}

/// The destination rect a replaced image paints into, placing the
/// [`object_fit_size`] result within `content` per `object-position`. Falls back
/// to `content` (the `fill` behaviour) when the style is absent or a size is
/// degenerate.
fn object_fit_dst(content: Rect, iw: u32, ih: u32, style: Option<&ComputedValues>) -> Rect {
    let Some(style) = style else {
        return content;
    };
    let (iw, ih) = (iw as f32, ih as f32);
    let (cw, ch) = (content.size.width, content.size.height);
    if iw <= 0.0 || ih <= 0.0 || cw <= 0.0 || ch <= 0.0 {
        return content;
    }
    let (ow, oh) = object_fit_size(style.clone_object_fit(), cw, ch, iw, ih);
    // `object-position` resolves like `background-position`: a percentage aligns
    // that fraction of the object with the same fraction of the free space
    // (`content − object`, which is negative when the object overflows), a length
    // is added on top — exactly `LengthPercentage::resolve`.
    let pos = style.clone_object_position();
    let off_x = pos.horizontal.resolve(Length::new(cw - ow)).px();
    let off_y = pos.vertical.resolve(Length::new(ch - oh)).px();
    Rect::from_xywh(content.origin.x + off_x, content.origin.y + off_y, ow, oh)
}

#[cfg(test)]
mod tests {
    use super::{ObjectFit, object_fit_size};

    #[test]
    fn fill_takes_the_whole_box_distorting_aspect() {
        assert_eq!(
            object_fit_size(ObjectFit::Fill, 200.0, 100.0, 40.0, 40.0),
            (200.0, 100.0)
        );
    }

    #[test]
    fn contain_fits_inside_preserving_aspect() {
        // A 2:1 image in a 100×100 box: width-limited, letterboxed vertically.
        assert_eq!(
            object_fit_size(ObjectFit::Contain, 100.0, 100.0, 20.0, 10.0),
            (100.0, 50.0)
        );
    }

    #[test]
    fn cover_fills_the_box_preserving_aspect() {
        // The same 2:1 image under `cover`: height-limited, overflows horizontally.
        assert_eq!(
            object_fit_size(ObjectFit::Cover, 100.0, 100.0, 20.0, 10.0),
            (200.0, 100.0)
        );
    }

    #[test]
    fn none_keeps_the_intrinsic_size() {
        assert_eq!(
            object_fit_size(ObjectFit::None, 100.0, 100.0, 20.0, 10.0),
            (20.0, 10.0)
        );
    }

    #[test]
    fn scale_down_uses_intrinsic_when_it_already_fits() {
        // Intrinsic (20×10) fits in the box → behaves as `none`, not `contain`.
        assert_eq!(
            object_fit_size(ObjectFit::ScaleDown, 100.0, 100.0, 20.0, 10.0),
            (20.0, 10.0)
        );
    }

    #[test]
    fn scale_down_shrinks_an_oversized_image_like_contain() {
        // Intrinsic (200×100) overflows → behaves as `contain`.
        assert_eq!(
            object_fit_size(ObjectFit::ScaleDown, 100.0, 100.0, 200.0, 100.0),
            (100.0, 50.0)
        );
    }
}
