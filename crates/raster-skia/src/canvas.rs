//! The display-list interpreter: walks items, maintaining clip and layer
//! stacks, and rasterizes fills, borders, and (WP-G) glyph runs onto a
//! `tiny_skia::Pixmap`.
//!
//! Opacity layers and rounded clips are the two unbounded-memory hazards
//! (ADR-0007 hardening, finding H1): a naive interpreter allocates a
//! *whole-canvas* surface per opacity layer and clones a *whole-canvas* mask
//! per clip, so deeply nested `opacity` / `overflow:hidden` amplify a tiny page
//! into gigabytes. Two defenses bound this:
//!
//! * Each opacity layer allocates a pixmap sized to the intersection of the
//!   current clip and the layer's device bounds (a sub-rect), not the whole
//!   canvas. A [`Surface`] therefore carries its device origin, and all drawing
//!   is expressed in surface-local coordinates.
//! * Hard caps on layer and per-surface clip nesting depth. Past the cap the
//!   layer / clip effect is skipped (drawn without it) rather than allocated,
//!   which degrades gracefully instead of exhausting memory.

use std::collections::HashMap;
use std::rc::Rc;

use oxidepage_base::{Rect, Transform2D};
use oxidepage_paint::{
    BorderEdge, BorderRadii, Brush, Color as PaintColor, DecodedImage, DisplayItem, DisplayList,
    FontId, ImageData, ImageId, PositionedGlyph, ResourceTable, TileMode, border_edge_quads,
    uniform_border_geometry,
};
use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, IntSize, LinearGradient, Mask, Paint, Pattern,
    Pixmap, PixmapPaint, Point, RadialGradient, Shader, SpreadMode, Transform,
};

use crate::glyphs::GlyphCache;
use crate::path;

/// Hard cap on opacity-layer nesting depth. Each layer allocates a pixmap
/// (bounded by the current clip), so unbounded nesting is a memory-amplification
/// hazard; past this depth the layer is composited without its opacity group.
const MAX_LAYER_DEPTH: usize = 128;

/// Hard cap on clip nesting depth per surface. Each clip holds a surface-sized
/// mask; past this depth the clip is ignored (drawing is not further clipped).
const MAX_CLIP_DEPTH: usize = 128;

/// Aggregate cap on bytes held by live layer pixmaps and clip masks at once.
///
/// The depth caps alone bound only the *count* of nested effects, not their
/// size: 128 unclipped full-canvas opacity layers plus 128 full-canvas clip
/// masks at 1920×1080 is already ~1.3 GB, and it scales with the area cap. Since
/// hostile HTML can author that nesting trivially (`<div style="opacity:.99">`
/// ×128), the total live allocation is bounded here too; past the budget an
/// effect is skipped exactly as at the depth cap. 256 MiB comfortably exceeds
/// any legitimate render (one full 64 MP canvas is already the area cap).
const MAX_EFFECT_BYTES: usize = 256 * 1024 * 1024;

/// Converts a paint color to a tiny-skia color.
fn skia_color(c: PaintColor) -> Color {
    Color::from_rgba8(c.r, c.g, c.b, c.a)
}

/// A CSS `matrix(a, b, c, d, tx, ty)` as a tiny-skia transform: both map
/// `(x, y)` to `(a·x + c·y + tx, b·x + d·y + ty)`.
fn skia_transform(t: Transform2D) -> Transform {
    Transform::from_row(t.a, t.b, t.c, t.d, t.tx, t.ty)
}

/// The axis-aligned bounds `(min_x, max_x, min_y, max_y)` of a set of points.
fn point_bounds(points: &[Point]) -> (f32, f32, f32, f32) {
    let (min_x, max_x) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| {
        (lo.min(p.x), hi.max(p.x))
    });
    let (min_y, max_y) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| {
        (lo.min(p.y), hi.max(p.y))
    });
    (min_x, max_x, min_y, max_y)
}

fn spread(extend: oxidepage_paint::ExtendMode) -> SpreadMode {
    match extend {
        oxidepage_paint::ExtendMode::Pad => SpreadMode::Pad,
        oxidepage_paint::ExtendMode::Repeat => SpreadMode::Repeat,
        oxidepage_paint::ExtendMode::Reflect => SpreadMode::Reflect,
    }
}

/// An integer device-pixel rectangle in *canvas* coordinates (origin at the
/// canvas top-left, before any surface offset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeviceRect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl DeviceRect {
    /// Intersection; a non-overlapping pair yields a zero-size rect.
    fn intersect(self, other: DeviceRect) -> DeviceRect {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.w).min(other.x + other.w);
        let y1 = (self.y + self.h).min(other.y + other.h);
        if x1 > x0 && y1 > y0 {
            DeviceRect {
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
            }
        } else {
            DeviceRect {
                x: x0,
                y: y0,
                w: 0,
                h: 0,
            }
        }
    }
}

/// One active clip: a mask (sized to its surface) plus the device bounding box
/// of the clipped region (in canvas coordinates), used to size sub-layers.
struct Clip {
    mask: Mask,
    bounds: DeviceRect,
    /// Bytes this mask charged to the effect budget (refunded when popped).
    bytes: usize,
}

/// A draw surface: the base canvas or an opacity layer. Layers are sized to a
/// sub-rect of the canvas and remember their device `origin` so drawing can be
/// expressed in surface-local coordinates.
struct Surface {
    pixmap: Pixmap,
    /// Device-pixel offset of this pixmap's (0, 0) within the canvas.
    origin_x: u32,
    origin_y: u32,
    /// Accumulated clip stack (real masks only); `clips.last()` is in effect.
    clips: Vec<Clip>,
    /// One marker per display-list `PushClip` on this surface: `true` if it
    /// pushed a real clip, `false` if it was skipped (depth cap / alloc
    /// failure). Keeps `PopClip` balanced without re-deriving which clip to pop.
    clip_markers: Vec<bool>,
    /// Opacity to composite this layer with (`1.0` for the base surface).
    opacity: f32,
    /// Bytes this layer's own pixmap charged to the effect budget (0 for the
    /// base surface, which is not an effect). Refunded when the layer is popped.
    pixmap_bytes: usize,
}

impl Surface {
    /// This surface's extent as a canvas-coordinate device rect.
    fn rect(&self) -> DeviceRect {
        DeviceRect {
            x: self.origin_x,
            y: self.origin_y,
            w: self.pixmap.width(),
            h: self.pixmap.height(),
        }
    }
}

/// The rasterization state: a stack of draw surfaces (base + opacity layers),
/// each with its own clip stack.
pub(crate) struct Canvas {
    surfaces: Vec<Surface>,
    /// One marker per display-list `PushLayer`: `true` if it pushed a real
    /// surface, `false` if skipped (depth cap / alloc failure).
    layer_markers: Vec<bool>,
    /// Device-pixel dimensions of the canvas (the base surface).
    width: u32,
    height: u32,
    /// CSS-px → device-px transform (device pixel ratio).
    scale: Transform,
    /// The current CSS `transform` matrix: CSS-px → CSS-px, the composition of
    /// every enclosing transformed box. The display list positions items in the
    /// *untransformed* absolute coordinate space, so this is what maps them.
    ctm: Transform,
    /// One saved CTM per `PushLayer` (restored by its `PopLayer`).
    ctm_stack: Vec<Transform>,
    /// Document (viewport) scroll offset in CSS px. The display list is built
    /// unscrolled; document-anchored content is translated by `-scroll` at draw
    /// time (innermost, i.e. before any CSS `transform`), reproducing the
    /// geometry a scroll-baked list would have had.
    scroll: oxidepage_base::Point,
    /// Nesting depth of viewport-anchored (`position: fixed`) subtrees, toggled
    /// by the `PushViewportAnchor` / `PopViewportAnchor` markers. While non-zero
    /// the document scroll is suppressed, pinning the content to the viewport.
    viewport_anchor: u32,
    /// Device pixel ratio as a scalar (for image tiling extents / bounds math).
    dpr: f32,
    /// Bytes currently held by layer pixmaps and clip masks, against
    /// [`MAX_EFFECT_BYTES`]. The base surface is excluded (it is not an effect).
    effect_bytes: usize,
    glyph_cache: GlyphCache,
    /// Premultiplied pixmaps, keyed by image id *and pixmap size*. Building one
    /// clones and premultiplies a whole RGBA buffer — and for a vector image
    /// runs resvg — while the display list is re-rastered on every scroll, so an
    /// uncached build would repeat that work per image per frame. `None`
    /// memoizes an image that cannot be built.
    ///
    /// The size is part of the key because a vector image is rasterized per
    /// device size: the same icon drawn at two sizes is two pixmaps. A raster
    /// image is always keyed at its intrinsic size, so it is still built once.
    /// The cache lives for one `Canvas`, i.e. one render — enough for a headless
    /// engine, whose renders are one-shot screenshots and PDFs.
    image_cache: HashMap<(ImageId, u32, u32), Option<Rc<Pixmap>>>,
}

impl Canvas {
    /// Builds a canvas with an opaque `background` base. Returns `None` if the
    /// base pixmap cannot be allocated (e.g. an absurd viewport × dpr); callers
    /// return a placeholder image rather than panicking (finding L3).
    pub(crate) fn new(
        width: u32,
        height: u32,
        scale: f32,
        background: PaintColor,
        scroll: oxidepage_base::Point,
    ) -> Option<Self> {
        let mut base = Pixmap::new(width, height)?;
        base.fill(skia_color(background));
        Some(Self {
            surfaces: vec![Surface {
                pixmap: base,
                origin_x: 0,
                origin_y: 0,
                clips: Vec::new(),
                clip_markers: Vec::new(),
                opacity: 1.0,
                pixmap_bytes: 0,
            }],
            layer_markers: Vec::new(),
            width,
            height,
            scale: Transform::from_scale(scale, scale),
            ctm: Transform::identity(),
            ctm_stack: Vec::new(),
            scroll,
            viewport_anchor: 0,
            dpr: scale,
            effect_bytes: 0,
            glyph_cache: GlyphCache::default(),
            image_cache: HashMap::new(),
        })
    }

    /// Reserves `bytes` of effect budget, returning false when it would exceed
    /// [`MAX_EFFECT_BYTES`].
    fn reserve_effect_bytes(&mut self, bytes: usize) -> bool {
        if self.effect_bytes.saturating_add(bytes) > MAX_EFFECT_BYTES {
            return false;
        }
        self.effect_bytes += bytes;
        true
    }

    pub(crate) fn run(&mut self, list: &DisplayList) {
        for item in &list.items {
            self.exec(item, &list.resources);
        }
    }

    /// The finished base pixmap (all layers popped by a balanced list).
    pub(crate) fn finish(mut self) -> Pixmap {
        self.surfaces.remove(0).pixmap
    }

    /// The current (top) draw surface.
    fn top(&mut self) -> &mut Surface {
        self.surfaces.last_mut().expect("surface stack non-empty")
    }

    /// The current surface's pixmap.
    fn surface(&mut self) -> &mut Pixmap {
        &mut self.top().pixmap
    }

    /// The document scroll to apply to the content currently being drawn: the
    /// live scroll for document-anchored content, zero inside a viewport anchor
    /// (`position: fixed`, which stays pinned).
    fn content_scroll(&self) -> oxidepage_base::Point {
        if self.viewport_anchor > 0 {
            oxidepage_base::Point::ZERO
        } else {
            self.scroll
        }
    }

    /// The CSS-px → surface-local device transform: the document scroll (applied
    /// innermost, so a CSS `transform` maps already-scrolled coordinates), then
    /// the CSS `transform` in effect, then the dpr scale, then a translation
    /// subtracting the current surface's device origin.
    fn draw_transform(&self) -> Transform {
        let s = self.surfaces.last().expect("surface stack non-empty");
        let scroll = self.content_scroll();
        self.scale
            .pre_concat(self.ctm)
            .pre_translate(-scroll.x, -scroll.y)
            .post_translate(-(s.origin_x as f32), -(s.origin_y as f32))
    }

    /// The canvas, in the coordinate space items are authored in (i.e. mapped
    /// back through the full draw transform's CSS part — the CTM *and* the
    /// document scroll). A tiled background inside a transformed box, or on
    /// scrolled document content, must still cover the whole visible canvas
    /// after that transform is applied.
    fn local_canvas_region(&self) -> Rect {
        let css = Rect::from_xywh(
            0.0,
            0.0,
            self.width as f32 / self.dpr,
            self.height as f32 / self.dpr,
        );
        // The draw transform maps item → device as `scale · ctm · translate(-scroll)`,
        // so the item-space region covering the canvas is `translate(scroll) · ctm⁻¹(css)`.
        let scroll = self.content_scroll();
        let Some(inverse) = self.ctm.invert() else {
            return Rect::from_xywh(
                css.min_x() + scroll.x,
                css.min_y() + scroll.y,
                css.size.width,
                css.size.height,
            );
        };
        let mut corners = [
            Point::from_xy(css.min_x(), css.min_y()),
            Point::from_xy(css.max_x(), css.min_y()),
            Point::from_xy(css.max_x(), css.max_y()),
            Point::from_xy(css.min_x(), css.max_y()),
        ];
        inverse.map_points(&mut corners);
        let (min_x, max_x, min_y, max_y) = point_bounds(&corners);
        Rect::from_xywh(
            min_x + scroll.x,
            min_y + scroll.y,
            max_x - min_x,
            max_y - min_y,
        )
    }

    /// A clone of the current surface's active clip mask, if any.
    fn top_clip(&self) -> Option<Mask> {
        self.surfaces
            .last()
            .and_then(|s| s.clips.last())
            .map(|c| c.mask.clone())
    }

    fn exec(&mut self, item: &DisplayItem, resources: &ResourceTable) {
        match item {
            DisplayItem::Fill { rect, radii, brush } => self.fill(*rect, radii, brush),
            DisplayItem::Border { rect, radii, edges } => self.border(*rect, radii, edges),
            DisplayItem::PushClip { rect, radii } => self.push_clip(*rect, radii),
            DisplayItem::PopClip => self.pop_clip(),
            DisplayItem::PushLayer { opacity, transform } => self.push_layer(*opacity, *transform),
            DisplayItem::PopLayer => self.pop_layer(),
            DisplayItem::GlyphRun {
                font,
                size,
                color,
                normalized_coords,
                glyphs,
                ..
            } => self.glyph_run(resources, *font, *size, *color, normalized_coords, glyphs),
            DisplayItem::Image {
                dst,
                image,
                tile,
                radii,
            } => self.image(resources, *dst, *image, *tile, radii),
            DisplayItem::PushViewportAnchor => self.viewport_anchor += 1,
            DisplayItem::PopViewportAnchor => {
                self.viewport_anchor = self.viewport_anchor.saturating_sub(1);
            }
        }
    }

    /// The premultiplied pixmap to blit for `id`, memoized (including build
    /// failures). A raster image yields its intrinsic-size pixels; a vector
    /// image is rasterized to `device`, the size it will occupy on the surface.
    fn image_pixmap(
        &mut self,
        id: ImageId,
        image: &DecodedImage,
        device: (u32, u32),
    ) -> Option<Rc<Pixmap>> {
        let key = match &image.data {
            ImageData::Raster { .. } => (id, image.width, image.height),
            ImageData::Vector { .. } => (id, device.0, device.1),
        };
        self.image_cache
            .entry(key)
            .or_insert_with(|| pixmap_from_image(image, device).map(Rc::new))
            .clone()
    }

    /// The size, in device pixels, that a `dst`-sized CSS rect occupies under
    /// the current draw transform. This is what a vector image is rasterized to
    /// — it already folds in the device pixel ratio and any CSS `transform`, so
    /// an icon under `scale(3)` at `dpr: 2` renders at 6× its CSS size rather
    /// than being blurrily upscaled. `None` when the transform is degenerate
    /// (a zero scale collapses the image to nothing).
    fn device_size(&self, dst: Rect) -> Option<(u32, u32)> {
        let t = self.draw_transform();
        // Column norms of the linear part: the length a unit x/y edge maps to.
        let sx = t.sx.hypot(t.ky);
        let sy = t.kx.hypot(t.sy);
        let (width, height) = (dst.size.width * sx, dst.size.height * sy);
        // A NaN or non-positive extent means there is nothing on screen to draw
        // into; `clamp_device_size` would round it up to a 1×1 pixmap.
        if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
            return None;
        }
        Some(crate::clamp_device_size(width.ceil(), height.ceil()))
    }

    fn image(
        &mut self,
        resources: &ResourceTable,
        dst: Rect,
        id: ImageId,
        tile: TileMode,
        radii: &BorderRadii,
    ) {
        let Some(image) = resources.image(id) else {
            return;
        };
        let Some(device) = self.device_size(dst) else {
            return;
        };
        let Some(pixmap) = self.image_pixmap(id, image, device) else {
            return;
        };
        // Map pixmap pixels onto `dst`. For a vector image the pixmap *is* the
        // device size, so this scale cancels the transform's and the blit is
        // device-pixel-for-device-pixel.
        let sx = dst.size.width / pixmap.width().max(1) as f32;
        let sy = dst.size.height / pixmap.height().max(1) as f32;

        match tile {
            TileMode::Stretch => {
                // Map image pixels → dst (CSS px) → surface-local device.
                let transform = self
                    .draw_transform()
                    .pre_translate(dst.origin.x, dst.origin.y)
                    .pre_scale(sx, sy);
                let clip = self.image_clip(dst, radii);
                let paint = PixmapPaint {
                    quality: FilterQuality::Bilinear,
                    ..PixmapPaint::default()
                };
                self.surface().draw_pixmap(
                    0,
                    0,
                    (*pixmap).as_ref(),
                    &paint,
                    transform,
                    clip.as_ref(),
                );
            }
            TileMode::Repeat | TileMode::RepeatX | TileMode::RepeatY => {
                // A tile-sized pattern fills the current clip (the builder
                // pushed the background/border box). RepeatX/Y are treated as
                // full repeat in v1 (ADR-0007 D7).
                let pattern_transform =
                    Transform::from_translate(dst.origin.x, dst.origin.y).pre_scale(sx, sy);
                let shader = Pattern::new(
                    (*pixmap).as_ref(),
                    SpreadMode::Repeat,
                    FilterQuality::Bilinear,
                    1.0,
                    pattern_transform,
                );
                let paint = Paint {
                    shader,
                    anti_alias: true,
                    ..Paint::default()
                };
                // Fill the whole viewport, bounded by the current clip and the
                // surface's own bounds.
                let region = self.local_canvas_region();
                let Some(path) = path::rounded_rect(region, &BorderRadii::ZERO) else {
                    return;
                };
                let transform = self.draw_transform();
                let clip = self.top_clip();
                self.surface().fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    transform,
                    clip.as_ref(),
                );
            }
        }
    }

    /// Builds a clip mask for the current surface: the current active clip (or a
    /// full-surface mask if none), intersected with the rounded `rect`. `None`
    /// only when the mask cannot be allocated (finding L3), so callers skip the
    /// clip gracefully. Shared by [`Self::push_clip`] and [`Self::image_clip`].
    fn build_clip_mask(&self, rect: Rect, radii: &BorderRadii) -> Option<Mask> {
        let surf = self.surfaces.last()?;
        let (w, h) = (surf.pixmap.width(), surf.pixmap.height());
        let mut mask = match surf.clips.last() {
            Some(existing) => existing.mask.clone(),
            None => {
                // A full mask (whole surface visible), then intersect.
                let mut m = Mask::new(w, h)?;
                if let Some(full) = path::rounded_rect(
                    Rect::from_xywh(0.0, 0.0, w as f32, h as f32),
                    &BorderRadii::ZERO,
                ) {
                    m.fill_path(&full, FillRule::Winding, true, Transform::identity());
                }
                m
            }
        };
        if let Some(p) = path::rounded_rect(rect, radii) {
            mask.intersect_path(&p, FillRule::Winding, true, self.draw_transform());
        }
        Some(mask)
    }

    /// The clip mask for a stretched image: the current clip intersected with
    /// the rounded destination when `radii` is non-zero, else the current clip.
    fn image_clip(&self, dst: Rect, radii: &BorderRadii) -> Option<Mask> {
        if radii.is_zero() {
            return self.top_clip();
        }
        self.build_clip_mask(dst, radii)
    }

    fn glyph_run(
        &mut self,
        resources: &ResourceTable,
        font: FontId,
        size: f32,
        color: PaintColor,
        coords: &[f32],
        glyphs: &[PositionedGlyph],
    ) {
        let mut paint = Paint {
            anti_alias: true,
            ..Paint::default()
        };
        paint.shader = Shader::SolidColor(skia_color(color));
        let base = self.draw_transform();
        // The clip is loop-invariant; clone it once rather than per glyph (each
        // clone is a full-surface mask, ~1 byte/px).
        let clip = self.top_clip();
        for g in glyphs {
            // The shared `Rc<Path>` releases the glyph-cache borrow (a refcount
            // bump, not a deep path clone) so the surface can be drawn onto.
            let Some(path) = self
                .glyph_cache
                .glyph_path(resources, font, g.id, size, coords)
            else {
                continue;
            };
            // The path is in glyph-local coordinates; place it at the glyph
            // pen origin, then apply the surface-local device transform.
            let transform = base.pre_translate(g.x, g.y);
            self.surface()
                .fill_path(&path, &paint, FillRule::Winding, transform, clip.as_ref());
        }
    }

    fn fill(&mut self, rect: Rect, radii: &BorderRadii, brush: &Brush) {
        let Some(p) = path::rounded_rect(rect, radii) else {
            return;
        };
        let mut paint = Paint {
            anti_alias: true,
            ..Paint::default()
        };
        match shader_for(brush) {
            Some(shader) => paint.shader = shader,
            None => return,
        }
        let transform = self.draw_transform();
        let clip = self.top_clip();
        self.surface()
            .fill_path(&p, &paint, FillRule::Winding, transform, clip.as_ref());
    }

    fn border(&mut self, rect: Rect, radii: &BorderRadii, edges: &[BorderEdge; 4]) {
        if !edges.iter().any(BorderEdge::paints) {
            return;
        }
        // Uniform-border detection and inner geometry are shared with the PDF
        // backend so the two outputs stay identical (ADR-0007 D7).
        if let Some((inner, inner_radii, edge)) = uniform_border_geometry(rect, radii, edges) {
            self.uniform_border(rect, radii, inner, &inner_radii, edge);
        } else {
            self.edge_borders(rect, edges);
        }
    }

    /// A uniform border: the ring between the border box and the padding box,
    /// filled with an even-odd rule so it honors rounded corners.
    fn uniform_border(
        &mut self,
        rect: Rect,
        radii: &BorderRadii,
        inner: Rect,
        inner_radii: &BorderRadii,
        edge: BorderEdge,
    ) {
        let mut pb = tiny_skia::PathBuilder::new();
        if let Some(outer) = path::rounded_rect(rect, radii) {
            pb.push_path(&outer);
        }
        if let Some(hole) = path::rounded_rect(inner, inner_radii) {
            pb.push_path(&hole);
        }
        let Some(ring) = pb.finish() else { return };

        let mut paint = Paint {
            anti_alias: true,
            ..Paint::default()
        };
        paint.shader = Shader::SolidColor(skia_color(edge.color));
        let transform = self.draw_transform();
        let clip = self.top_clip();
        self.surface()
            .fill_path(&ring, &paint, FillRule::EvenOdd, transform, clip.as_ref());
    }

    /// Non-uniform borders: one trapezoid per painting edge (rectangular; the
    /// corners are mitered, radii ignored in this v1 case). Trapezoid geometry
    /// is shared with the PDF backend via [`border_edge_quads`].
    fn edge_borders(&mut self, rect: Rect, edges: &[BorderEdge; 4]) {
        let transform = self.draw_transform();
        for (edge, pts) in border_edge_quads(rect, edges) {
            if !edge.paints() {
                continue;
            }
            let Some(quad) = path::quad(pts[0], pts[1], pts[2], pts[3]) else {
                continue;
            };
            let mut paint = Paint {
                anti_alias: true,
                ..Paint::default()
            };
            paint.shader = Shader::SolidColor(skia_color(edge.color));
            let clip = self.top_clip();
            self.surface()
                .fill_path(&quad, &paint, FillRule::Winding, transform, clip.as_ref());
        }
    }

    /// The device bounding box (canvas coordinates) of a CSS-px rect, after the
    /// document scroll and the CTM: the rect is authored in unscrolled item
    /// space, so the same `translate(-scroll)` then CTM the draw transform
    /// applies must map it here too, or a scrolled/transformed clip would size
    /// and place its nested layers wrong.
    fn device_bbox(&self, rect: Rect) -> DeviceRect {
        let scroll = self.content_scroll();
        let mut corners = [
            Point::from_xy(rect.min_x() - scroll.x, rect.min_y() - scroll.y),
            Point::from_xy(rect.max_x() - scroll.x, rect.min_y() - scroll.y),
            Point::from_xy(rect.max_x() - scroll.x, rect.max_y() - scroll.y),
            Point::from_xy(rect.min_x() - scroll.x, rect.max_y() - scroll.y),
        ];
        self.ctm.map_points(&mut corners);
        let (lo_x, hi_x, lo_y, hi_y) = point_bounds(&corners);

        let clamp_x = |v: f32| v.clamp(0.0, self.width as f32);
        let clamp_y = |v: f32| v.clamp(0.0, self.height as f32);
        let x0 = clamp_x((lo_x * self.dpr).floor());
        let y0 = clamp_y((lo_y * self.dpr).floor());
        let x1 = clamp_x((hi_x * self.dpr).ceil());
        let y1 = clamp_y((hi_y * self.dpr).ceil());
        DeviceRect {
            x: x0 as u32,
            y: y0 as u32,
            w: (x1 - x0).max(0.0) as u32,
            h: (y1 - y0).max(0.0) as u32,
        }
    }

    /// The canvas-coordinate bounds of a new clip: the rect's device bbox
    /// intersected with the current clip (or the current surface's extent).
    fn new_clip_bounds(&self, rect: Rect) -> DeviceRect {
        let dev = self.device_bbox(rect);
        let surf = self.surfaces.last().expect("surface stack non-empty");
        match surf.clips.last() {
            Some(c) => dev.intersect(c.bounds),
            None => dev.intersect(surf.rect()),
        }
    }

    fn push_clip(&mut self, rect: Rect, radii: &BorderRadii) {
        // Depth cap: past it, ignore the clip but record the push so the
        // matching PopClip stays balanced.
        if self.top().clips.len() >= MAX_CLIP_DEPTH {
            self.top().clip_markers.push(false);
            return;
        }
        // Each mask is surface-sized (1 byte/px). Skip the clip if allocating
        // its mask would push live effect memory past the aggregate budget.
        let surf = self.top();
        let mask_bytes =
            (surf.pixmap.width() as usize).saturating_mul(surf.pixmap.height() as usize);
        if !self.reserve_effect_bytes(mask_bytes) {
            self.top().clip_markers.push(false);
            return;
        }
        let bounds = self.new_clip_bounds(rect);
        match self.build_clip_mask(rect, radii) {
            Some(mask) => {
                let surf = self.top();
                surf.clips.push(Clip {
                    mask,
                    bounds,
                    bytes: mask_bytes,
                });
                surf.clip_markers.push(true);
            }
            None => {
                self.effect_bytes = self.effect_bytes.saturating_sub(mask_bytes);
                self.top().clip_markers.push(false);
            }
        }
    }

    fn pop_clip(&mut self) {
        let surf = self.top();
        if let Some(true) = surf.clip_markers.pop()
            && let Some(clip) = surf.clips.pop()
        {
            let bytes = clip.bytes;
            self.effect_bytes = self.effect_bytes.saturating_sub(bytes);
        }
    }

    /// The device sub-rect a new layer needs: the current clip's bounds (or the
    /// current surface's extent when unclipped). Sizing the layer to this rather
    /// than the whole canvas is the core memory bound (finding H1).
    fn layer_bounds(&self) -> DeviceRect {
        let surf = self.surfaces.last().expect("surface stack non-empty");
        match surf.clips.last() {
            Some(c) => c.bounds.intersect(surf.rect()),
            None => surf.rect(),
        }
    }

    /// Crops the current surface's active clip into `sub` (canvas coordinates),
    /// producing a clip sized to the new layer surface so a rounded outer clip
    /// is still honored inside the layer. `None` when the surface is unclipped.
    fn crop_parent_clip(&self, sub: DeviceRect) -> Option<Clip> {
        let parent = self.surfaces.last()?;
        let pc = parent.clips.last()?;
        let mw = parent.pixmap.width();
        let mh = parent.pixmap.height();
        // `sub` is within the parent surface (layer_bounds intersects it), so
        // these offsets are non-negative.
        let px = sub.x - parent.origin_x;
        let py = sub.y - parent.origin_y;
        let src = pc.mask.data();
        let mut dst = vec![0u8; (sub.w as usize) * (sub.h as usize)];
        for row in 0..sub.h {
            let sy = py + row;
            if sy >= mh {
                break;
            }
            let copy_w = sub.w.min(mw.saturating_sub(px)) as usize;
            if copy_w == 0 {
                continue;
            }
            let src_start = (sy * mw + px) as usize;
            let dst_start = (row * sub.w) as usize;
            dst[dst_start..dst_start + copy_w].copy_from_slice(&src[src_start..src_start + copy_w]);
        }
        let bytes = dst.len();
        let mask = Mask::from_vec(dst, IntSize::from_wh(sub.w, sub.h)?)?;
        Some(Clip {
            mask,
            bounds: sub,
            bytes,
        })
    }

    /// Enters a layer: its `transform` joins the CTM for the whole subtree, and
    /// a translucent layer additionally gets its own surface to composite. A
    /// purely transformed (opaque) layer needs no surface, only the CTM.
    fn push_layer(&mut self, opacity: f32, transform: Transform2D) {
        self.ctm_stack.push(self.ctm);
        if transform != Transform2D::IDENTITY {
            self.ctm = self.ctm.pre_concat(skia_transform(transform));
        }
        if opacity >= 1.0 {
            self.layer_markers.push(false);
            return;
        }
        // Depth cap: past it, composite without an opacity group.
        if self.surfaces.len() > MAX_LAYER_DEPTH {
            self.layer_markers.push(false);
            return;
        }
        let sub = self.layer_bounds();
        let (sw, sh) = (sub.w.max(1), sub.h.max(1));
        // Aggregate-bytes cap: the depth cap bounds the count of layers, not
        // their combined size, so a stack of full-canvas layers is charged here.
        let pixmap_bytes = (sw as usize).saturating_mul(sh as usize).saturating_mul(4);
        if !self.reserve_effect_bytes(pixmap_bytes) {
            self.layer_markers.push(false);
            return;
        }
        let Some(pixmap) = Pixmap::new(sw, sh) else {
            // Allocation failed (absurd size): skip the layer gracefully.
            self.effect_bytes = self.effect_bytes.saturating_sub(pixmap_bytes);
            self.layer_markers.push(false);
            return;
        };
        // The seed clip (the parent clip cropped to the layer) is charged too;
        // if the budget can't cover it, leave the layer unclipped rather than
        // skip the whole opacity group.
        let seed = self
            .crop_parent_clip(sub)
            .filter(|seed| self.reserve_effect_bytes(seed.bytes));
        let clips = seed.into_iter().collect();
        self.surfaces.push(Surface {
            pixmap,
            origin_x: sub.x,
            origin_y: sub.y,
            clips,
            clip_markers: Vec::new(),
            opacity: opacity.clamp(0.0, 1.0),
            pixmap_bytes,
        });
        self.layer_markers.push(true);
    }

    fn pop_layer(&mut self) {
        if let Some(previous) = self.ctm_stack.pop() {
            self.ctm = previous;
        }
        if self.layer_markers.pop() != Some(true) {
            return; // skipped layer (or unbalanced list)
        }
        let Some(layer) = self.surfaces.pop() else {
            return;
        };
        // Refund the layer's pixmap and any clips still on it (the seed clip
        // never had a `PushClip`, so `pop_clip` didn't refund it).
        let freed = layer.pixmap_bytes + layer.clips.iter().map(|c| c.bytes).sum::<usize>();
        self.effect_bytes = self.effect_bytes.saturating_sub(freed);
        let paint = PixmapPaint {
            opacity: layer.opacity,
            ..PixmapPaint::default()
        };
        let parent = self.top();
        let dx = layer.origin_x as i32 - parent.origin_x as i32;
        let dy = layer.origin_y as i32 - parent.origin_y as i32;
        // Composite the sub-rect layer at its offset; the outer clip is already
        // baked into the layer's contents (seeded + drawn), so no mask here.
        parent.pixmap.draw_pixmap(
            dx,
            dy,
            layer.pixmap.as_ref(),
            &paint,
            Transform::identity(),
            None,
        );
    }
}

/// Builds the tiny-skia shader for a brush. A gradient with a single stop
/// collapses to a solid fill of that stop's color (matching the PDF backend,
/// finding L2); `None` means the brush cannot paint (a zero-stop gradient).
fn shader_for(brush: &Brush) -> Option<Shader<'static>> {
    match brush {
        Brush::Solid(c) => Some(Shader::SolidColor(skia_color(*c))),
        Brush::LinearGradient(g) => {
            if let Some(c) = single_stop(&g.stops) {
                return Some(Shader::SolidColor(skia_color(c)));
            }
            let stops = gradient_stops(&g.stops)?;
            LinearGradient::new(
                Point::from_xy(g.start.x, g.start.y),
                Point::from_xy(g.end.x, g.end.y),
                stops,
                spread(g.extend),
                Transform::identity(),
            )
        }
        Brush::RadialGradient(g) => {
            if let Some(c) = single_stop(&g.stops) {
                return Some(Shader::SolidColor(skia_color(c)));
            }
            let stops = gradient_stops(&g.stops)?;
            let rx = g.radius.width.max(f32::EPSILON);
            let ry = g.radius.height.max(f32::EPSILON);
            // Circle of radius rx, scaled on y to make an ellipse, about center.
            let transform = Transform::from_translate(g.center.x, g.center.y)
                .pre_scale(1.0, ry / rx)
                .pre_translate(-g.center.x, -g.center.y);
            RadialGradient::new(
                Point::from_xy(g.center.x, g.center.y),
                0.0,
                Point::from_xy(g.center.x, g.center.y),
                rx,
                stops,
                spread(g.extend),
                transform,
            )
        }
    }
}

/// The lone stop's color when a gradient has exactly one stop (degenerate:
/// a constant color across the whole shape).
fn single_stop(stops: &[oxidepage_paint::GradientStop]) -> Option<PaintColor> {
    match stops {
        [only] => Some(only.color),
        _ => None,
    }
}

/// Converts display-list gradient stops to tiny-skia stops (needs ≥ 2; the
/// single-stop case is handled by [`single_stop`]).
fn gradient_stops(stops: &[oxidepage_paint::GradientStop]) -> Option<Vec<GradientStop>> {
    if stops.len() < 2 {
        return None;
    }
    Some(
        stops
            .iter()
            .map(|s| GradientStop::new(s.offset.clamp(0.0, 1.0), skia_color(s.color)))
            .collect(),
    )
}

/// Builds the tiny-skia pixmap to blit for a stored image: a raster image's own
/// pixels, or a vector image rasterized to `device` (the size it occupies on the
/// surface). Either way the straight-alpha RGBA is premultiplied, which is how
/// tiny-skia stores pixels.
fn pixmap_from_image(image: &DecodedImage, device: (u32, u32)) -> Option<Pixmap> {
    match &image.data {
        ImageData::Raster { rgba } => premultiplied(rgba, image.width, image.height),
        ImageData::Vector { svg } => {
            let pixels = oxidepage_paint::rasterize_svg(svg, device.0, device.1)?;
            premultiplied(&pixels.rgba, pixels.width, pixels.height)
        }
    }
}

/// Premultiplies straight-alpha RGBA into a pixmap.
fn premultiplied(rgba: &[u8], width: u32, height: u32) -> Option<Pixmap> {
    let mut data = rgba.to_vec();
    for px in data.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        px[0] = (u32::from(px[0]) * a / 255) as u8;
        px[1] = (u32::from(px[1]) * a / 255) as u8;
        px[2] = (u32::from(px[2]) * a / 255) as u8;
    }
    Pixmap::from_vec(data, IntSize::from_wh(width, height)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepage_base::Size;
    use std::sync::Arc;

    fn checker() -> DecodedImage {
        DecodedImage {
            id: ImageId(7),
            width: 2,
            height: 2,
            data: ImageData::Raster {
                #[rustfmt::skip]
                rgba: Arc::new(vec![
                    255, 0, 0, 255,   0, 255, 0, 255,
                    0, 0, 255, 255,   255, 255, 255, 255,
                ]),
            },
        }
    }

    /// A raster image's premultiplied pixmap is built once per id, whatever the
    /// device size it is drawn at: a second draw reuses the cached `Rc` instead
    /// of re-cloning and re-premultiplying the whole RGBA buffer. Only a vector
    /// image, which rasterizes to the device size, is keyed by that size.
    #[test]
    fn raster_pixmap_is_cached_per_id_across_device_sizes() {
        let mut canvas = Canvas::new(16, 16, 1.0, PaintColor::WHITE, oxidepage_base::Point::ZERO)
            .expect("canvas");
        let image = checker();
        let first = canvas
            .image_pixmap(ImageId(7), &image, (10, 10))
            .expect("built");
        let second = canvas
            .image_pixmap(ImageId(7), &image, (40, 40))
            .expect("cached");
        assert!(
            Rc::ptr_eq(&first, &second),
            "a raster image reuses one cached pixmap regardless of draw size"
        );
        assert_eq!(canvas.image_cache.len(), 1, "one entry per raster image");
    }

    /// The device size folds in the canvas scale (dpr) and any CSS transform, so
    /// a 50×50 CSS rect at `dpr: 2` is 100×100 device pixels — the resolution a
    /// vector image is rasterized at.
    #[test]
    fn device_size_includes_the_canvas_scale() {
        let canvas = Canvas::new(
            200,
            200,
            2.0,
            PaintColor::WHITE,
            oxidepage_base::Point::ZERO,
        )
        .expect("canvas");
        assert_eq!(
            canvas.device_size(Rect::from_xywh(0.0, 0.0, 50.0, 50.0)),
            Some((100, 100))
        );
    }

    /// A degenerate (zero-scale) transform collapses the image to nothing: no
    /// pixmap is requested, rather than a 1×1 rasterization being blown up.
    #[test]
    fn device_size_rejects_a_collapsed_rect() {
        let canvas = Canvas::new(
            200,
            200,
            1.0,
            PaintColor::WHITE,
            oxidepage_base::Point::ZERO,
        )
        .expect("canvas");
        assert_eq!(
            canvas.device_size(Rect::from_xywh(0.0, 0.0, 0.0, 50.0)),
            None
        );
    }

    fn empty_list(w: f32, h: f32) -> DisplayList {
        DisplayList {
            viewport: Size::new(w, h),
            content_size: Size::new(w, h),
            items: Vec::new(),
            resources: ResourceTable::default(),
        }
    }

    /// A clip then a layer: the layer pixmap is sized to the clip sub-rect, not
    /// the whole canvas (finding H1 — the core memory bound).
    #[test]
    fn layer_pixmap_tracks_the_clip_not_the_canvas() {
        let mut canvas = Canvas::new(
            200,
            200,
            1.0,
            PaintColor::WHITE,
            oxidepage_base::Point::ZERO,
        )
        .expect("canvas");
        canvas.push_clip(Rect::from_xywh(10.0, 20.0, 40.0, 30.0), &BorderRadii::ZERO);
        canvas.push_layer(0.5, Transform2D::IDENTITY);
        let layer = canvas.surfaces.last().expect("layer surface");
        assert_eq!(
            (layer.pixmap.width(), layer.pixmap.height()),
            (40, 30),
            "layer sized to the clip sub-rect, not 200×200"
        );
        assert_eq!((layer.origin_x, layer.origin_y), (10, 20));
    }

    /// An unclipped layer falls back to the surface extent (no clip to shrink
    /// to), and the depth cap still bounds how many can stack.
    #[test]
    fn layer_depth_is_capped() {
        let mut canvas = Canvas::new(64, 64, 1.0, PaintColor::WHITE, oxidepage_base::Point::ZERO)
            .expect("canvas");
        // Push far more layers than the cap; excess ones must not allocate.
        for _ in 0..(MAX_LAYER_DEPTH + 50) {
            canvas.push_layer(0.9, Transform2D::IDENTITY);
        }
        assert_eq!(
            canvas.surfaces.len(),
            MAX_LAYER_DEPTH + 1,
            "surfaces bounded by the cap (+ base)"
        );
        // Balanced pops leave just the base.
        for _ in 0..(MAX_LAYER_DEPTH + 50) {
            canvas.pop_layer();
        }
        assert_eq!(canvas.surfaces.len(), 1, "back to the base surface");
    }

    /// Clip depth is capped per surface; excess clips allocate no mask.
    #[test]
    fn clip_depth_is_capped() {
        let mut canvas = Canvas::new(64, 64, 1.0, PaintColor::WHITE, oxidepage_base::Point::ZERO)
            .expect("canvas");
        for _ in 0..(MAX_CLIP_DEPTH + 50) {
            canvas.push_clip(Rect::from_xywh(0.0, 0.0, 32.0, 32.0), &BorderRadii::ZERO);
        }
        assert_eq!(
            canvas.surfaces[0].clips.len(),
            MAX_CLIP_DEPTH,
            "real clip masks bounded by the cap"
        );
        for _ in 0..(MAX_CLIP_DEPTH + 50) {
            canvas.pop_clip();
        }
        assert!(canvas.surfaces[0].clips.is_empty(), "all clips popped");
    }

    /// Deeply nested layers + clips complete without exhausting memory: with
    /// N-per-level full-canvas allocations this would be gigabytes.
    #[test]
    fn deep_nesting_is_bounded() {
        let mut canvas = Canvas::new(
            256,
            256,
            1.0,
            PaintColor::WHITE,
            oxidepage_base::Point::ZERO,
        )
        .expect("canvas");
        let list = empty_list(256.0, 256.0);
        let mut items = Vec::new();
        for i in 0..500 {
            items.push(DisplayItem::PushLayer {
                opacity: 0.5,
                transform: oxidepage_base::Transform2D::IDENTITY,
            });
            let inset = (i % 100) as f32;
            items.push(DisplayItem::PushClip {
                rect: Rect::from_xywh(inset, inset, 100.0, 100.0),
                radii: BorderRadii::ZERO,
            });
        }
        for _ in 0..500 {
            items.push(DisplayItem::PopClip);
            items.push(DisplayItem::PopLayer);
        }
        let list = DisplayList { items, ..list };
        canvas.run(&list);
        // Balanced: everything popped back to the base surface.
        assert_eq!(canvas.surfaces.len(), 1);
    }

    /// Regression: the depth caps bound the *count* of nested effects, not their
    /// size. Unclipped full-canvas opacity layers each allocate a whole-canvas
    /// pixmap, so a stack of them is gigabytes even under the depth cap. The
    /// aggregate-bytes budget must cap the live total and skip the excess.
    ///
    /// Only the push side is exercised (the layers are dropped with the canvas):
    /// pop-time compositing of hundreds of MiB is slow in a debug build and not
    /// what this test is about.
    #[test]
    fn unclipped_layer_nesting_is_bounded_by_the_byte_budget() {
        let side = 2048u32; // 16 MiB per full-canvas layer
        let mut canvas = Canvas::new(
            side,
            side,
            1.0,
            PaintColor::WHITE,
            oxidepage_base::Point::ZERO,
        )
        .expect("canvas");

        let mut peak_surfaces = 1;
        for _ in 0..MAX_LAYER_DEPTH {
            canvas.push_layer(0.99, Transform2D::IDENTITY);
            peak_surfaces = peak_surfaces.max(canvas.surfaces.len());
            assert!(
                canvas.effect_bytes <= MAX_EFFECT_BYTES,
                "live effect memory never exceeds the budget"
            );
        }

        // The byte budget — not the depth cap — is what stopped the nesting.
        let per_layer = (side as usize).pow(2) * 4;
        assert!(
            peak_surfaces <= 1 + MAX_EFFECT_BYTES / per_layer,
            "peaked at {peak_surfaces} live surfaces — budget did not bound the stack"
        );
        assert!(
            peak_surfaces < 1 + MAX_LAYER_DEPTH,
            "the depth cap alone would have allowed far more"
        );
    }

    /// Every layer and clip reservation is refunded once popped, so the counter
    /// returns to zero and a later render is not starved. Small surface: the
    /// pop-time compositing is what makes this cheap enough to also exercise.
    #[test]
    fn effect_budget_is_refunded_on_pop() {
        let mut canvas = Canvas::new(
            256,
            256,
            1.0,
            PaintColor::WHITE,
            oxidepage_base::Point::ZERO,
        )
        .expect("canvas");
        let list = empty_list(256.0, 256.0);
        let items = vec![
            DisplayItem::PushLayer {
                opacity: 0.5,
                transform: oxidepage_base::Transform2D::IDENTITY,
            },
            DisplayItem::PushClip {
                rect: Rect::from_xywh(0.0, 0.0, 128.0, 128.0),
                radii: BorderRadii::ZERO,
            },
            DisplayItem::PopClip,
            DisplayItem::PopLayer,
        ];
        canvas.run(&DisplayList { items, ..list });
        assert_eq!(
            canvas.effect_bytes, 0,
            "the layer and clip reservations are both refunded"
        );
        assert_eq!(canvas.surfaces.len(), 1);
    }
}
