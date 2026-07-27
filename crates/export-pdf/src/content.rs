//! Builds a PDF content stream from a display list and collects the resource
//! objects (image XObjects, gradient shadings, opacity ExtGStates, embedded
//! fonts) it refers to (ADR-0007 D3/M, ADR-0008). Coordinates are emitted in
//! CSS px under a base CTM that scales to points (×0.75) and flips the y axis.
//!
//! Text from TrueType fonts is emitted as real, selectable text: glyphs are
//! shown as 2-byte CIDs of a subset `Type0`/`CIDFontType2` font (Phase 7,
//! WP-E), positioned per glyph via the text matrix (which re-flips y so the
//! glyphs are upright under the page's y-flip CTM). Non-TrueType (CFF/OTF)
//! fonts fall back to Phase 6 vector outlines (rendered, not selectable).

use std::collections::HashMap;
use std::sync::Arc;

use oxidepage_base::{Point, Rect, Transform2D};
use oxidepage_paint::{
    BorderEdge, BorderRadii, Brush, Color, DecodedImage, DisplayItem, DisplayList, ExtendMode,
    FontId, ImageData, LinearGradient, PathCommand, PathSink, PositionedGlyph, RadialGradient,
    ResourceTable, border_edge_quads, emit_path_commands, emit_rounded_rect, glyph_outline,
    uniform_border_geometry,
};
use pdf_writer::types::FunctionShadingType;
use pdf_writer::{Content, Name, Ref, Str};
use subsetter::GlyphRemapper;

/// Device pixels per CSS px a vector image is rasterized at for embedding. PDF
/// has no vector-image XObject we can hand SVG to, so the SVG becomes a raster
/// XObject and the only question is at what resolution: ×3 is ~288 dpi against
/// the 96 dpi CSS px, which prints cleanly without inflating the file.
const PDF_SVG_SCALE: f32 = 3.0;

/// Caps on an embedded image's raster size, mirroring the decoder's. A vector
/// image's device size is derived from the (page-controlled) destination rect,
/// so it needs the same bound before a pixmap is allocated.
const MAX_PDF_IMAGE_SIDE: u32 = 16_384;
const MAX_PDF_IMAGE_PIXELS: u64 = 40_000_000;

/// An image to embed: the RGB XObject, its soft-mask (alpha) XObject, and the
/// straight-alpha RGBA pixels to write into them.
pub(crate) struct ImageSpec {
    pub name: Vec<u8>,
    pub obj: Ref,
    pub smask: Ref,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

/// A gradient to embed as an axial/radial function shading. The color ramp is a
/// PDF stitching (Type 3) function over every stop when there are ≥ 2 segments,
/// or a single exponential (Type 2) when there is exactly one — so all
/// intermediate stops and their offsets are honored, not just the endpoints
/// (finding M1).
pub(crate) struct ShadingSpec {
    pub name: Vec<u8>,
    pub obj: Ref,
    /// The top-level color function: the stitching parent, or (for a single
    /// segment) the sole exponential.
    pub function: Ref,
    /// One `Ref` per segment (adjacent stop pair) when stitching; empty when a
    /// single exponential is used directly as `function`.
    pub segments: Vec<Ref>,
    pub kind: FunctionShadingType,
    pub coords: Vec<f32>,
    /// The gradient's color ramp: `(offset, rgb)` sorted and augmented to span
    /// `[0, 1]` (a constant leading/trailing segment is added when the first/
    /// last stop is not at 0/1, so offsets are preserved). Always ≥ 2 entries.
    pub stops: Vec<(f32, [f32; 3])>,
    /// The PDF shading `/Extend` flags.
    pub extend: [bool; 2],
}

/// A constant-alpha ExtGState for an opacity layer.
pub(crate) struct GstateSpec {
    pub name: Vec<u8>,
    pub obj: Ref,
    pub alpha: f32,
}

/// A transparency-group form XObject holding a translucent layer's content, so
/// the layer's `opacity` composites the whole group once (true group opacity)
/// instead of darkening overlapping children. Drawn by the parent stream under
/// an [`GstateSpec`] carrying `ca`/`CA`.
pub(crate) struct FormSpec {
    pub name: Vec<u8>,
    pub obj: Ref,
    /// The buffered content-stream bytes for the layer's children.
    pub content: Vec<u8>,
    /// The form's `/BBox` in CSS px: `[x0, y0, x1, y1]`.
    pub bbox: [f32; 4],
}

/// An embedded, subsetted font referenced by the content stream (Phase 7,
/// WP-E). Carries the raw sfnt bytes plus the glyph remapper accumulated while
/// emitting text, so [`crate::fonts`] can subset and write the font objects.
pub(crate) struct FontUsage {
    /// Resource name in the page's `/Font` dictionary (e.g. `F0`).
    pub name: Vec<u8>,
    pub type0: Ref,
    pub cid_font: Ref,
    pub descriptor: Ref,
    pub font_file: Ref,
    pub cid_to_gid: Ref,
    pub to_unicode: Ref,
    /// The font this embeds; [`crate::fonts::write_font`] borrows the raw sfnt
    /// bytes from the resource table's shared blob (no per-font copy).
    pub font: FontId,
    /// Maps original glyph ids to the compact subset gid space; the subset's
    /// gids are used directly as CIDs (Identity encoding).
    pub remapper: GlyphRemapper,
}

/// The built content stream plus the resource objects it references.
pub(crate) struct Built {
    pub content: Vec<u8>,
    pub images: Vec<ImageSpec>,
    pub shadings: Vec<ShadingSpec>,
    pub gstates: Vec<GstateSpec>,
    pub fonts: Vec<FontUsage>,
    pub forms: Vec<FormSpec>,
}

/// A layer currently open on the graphics-state stack. `Group` layers buffer
/// their content into a form XObject so a translucent layer's opacity composites
/// the whole group once; `Plain` layers (fully opaque) are emitted inline and
/// only carry the CSS transform.
enum LayerState {
    Plain,
    Group {
        parent: Content,
        opacity: f32,
        transform: Transform2D,
    },
}

struct Builder {
    content: Content,
    images: Vec<ImageSpec>,
    shadings: Vec<ShadingSpec>,
    gstates: Vec<GstateSpec>,
    fonts: Vec<FontUsage>,
    forms: Vec<FormSpec>,
    /// Open layers, innermost last. A `Group` frame owns the parent content
    /// stream while its own children buffer into `self.content`.
    layers: Vec<LayerState>,
    /// Registered XObjects, keyed by image id and embedded raster size (a vector
    /// image is embedded once per size it is drawn at).
    image_names: HashMap<(u64, u32, u32), Vec<u8>>,
    /// Index into `fonts` for each font already registered this page.
    font_index: HashMap<FontId, usize>,
    /// The document box in CSS px, used as the `/BBox` of layer form XObjects.
    /// Passed in by `crate::export`, which is the one place it is derived.
    doc_w: f32,
    doc_h: f32,
    next_ref: i32,
}

/// Builds the content stream for `list`, assigning resource object ids starting
/// at `first_ref`. `scale` is the CSS-px → pt factor, and `doc_w`/`doc_h` the
/// document box in CSS px.
///
/// The stream opens with a base CTM that maps CSS px (y-down, origin at the
/// document's top-left) onto points (y-up, origin at the document's
/// bottom-left) — the document's own space, not a page's. A paginated export
/// wraps the whole thing in one form XObject and each page places a slice of it.
pub(crate) fn build(
    list: &DisplayList,
    scale: f32,
    first_ref: i32,
    doc_w: f32,
    doc_h: f32,
) -> Built {
    let doc_h_pt = doc_h * scale;

    let mut builder = Builder {
        content: Content::new(),
        images: Vec::new(),
        shadings: Vec::new(),
        gstates: Vec::new(),
        fonts: Vec::new(),
        forms: Vec::new(),
        layers: Vec::new(),
        image_names: HashMap::new(),
        font_index: HashMap::new(),
        doc_w,
        doc_h,
        next_ref: first_ref,
    };
    // Base CTM: CSS px (y-down) → PDF pt (y-up).
    builder
        .content
        .transform([scale, 0.0, 0.0, -scale, 0.0, doc_h_pt]);
    for item in &list.items {
        builder.exec(item, &list.resources);
    }

    Built {
        content: builder.content.finish().to_vec(),
        images: builder.images,
        shadings: builder.shadings,
        gstates: builder.gstates,
        fonts: builder.fonts,
        forms: builder.forms,
    }
}

/// True when `data`/`index` is a single-face TrueType font that actually
/// carries a `glyf` table, so it can be embedded as a `CIDFontType2` with a
/// valid `FontFile2`. This deliberately rejects CFF (`OTTO`, or a `CFF ` table
/// under a TrueType version tag), TrueType collections (`ttcf` — a whole
/// collection is not a valid single-font `FontFile2`), and any blob skrifa
/// cannot parse; all of those fall back to outline emission (ADR-0008). The
/// parse+`glyf` check also guarantees [`crate::fonts::write_font`] never bails
/// after the content stream has already committed to embedding this font.
fn is_embeddable_truetype(data: &[u8], index: u32) -> bool {
    if !matches!(
        data.get(0..4),
        Some([0x00, 0x01, 0x00, 0x00]) | Some(b"true")
    ) {
        return false;
    }
    skrifa::FontRef::from_index(data, index)
        .map(|font| font.table_data(skrifa::Tag::new(b"glyf")).is_some())
        .unwrap_or(false)
}

/// sRGB 0..255 → 0..1.
fn norm(c: Color) -> [f32; 3] {
    [
        f32::from(c.r) / 255.0,
        f32::from(c.g) / 255.0,
        f32::from(c.b) / 255.0,
    ]
}

/// Normalizes a gradient's stops into the color ramp used to build the PDF
/// stitching function: offsets clamped to `[0, 1]` and forced non-decreasing,
/// then augmented so the ramp spans the full `[0, 1]` shading domain. If the
/// first/last stop is not at 0/1 a constant leading/trailing segment is added,
/// which is exactly the CSS behavior (the color is held before the first stop
/// and after the last) — and what preserves the stops' offsets. Always returns
/// ≥ 2 entries.
fn norm_stops(stops: &[oxidepage_paint::GradientStop]) -> Vec<(f32, [f32; 3])> {
    let mut ramp: Vec<(f32, [f32; 3])> = Vec::with_capacity(stops.len() + 2);
    let mut max = 0.0f32;
    for s in stops {
        let offset = s.offset.clamp(0.0, 1.0).max(max);
        max = offset;
        ramp.push((offset, norm(s.color)));
    }
    if ramp.is_empty() {
        return vec![(0.0, [0.0; 3]), (1.0, [0.0; 3])];
    }
    if ramp[0].0 > 0.0 {
        ramp.insert(0, (0.0, ramp[0].1));
    }
    let last = ramp.len() - 1;
    if ramp[last].0 < 1.0 {
        ramp.push((1.0, ramp[last].1));
    }
    ramp
}

/// A gradient shading's parameters: the shading kind, its `/Coords` array, the
/// color ramp as `(offset, rgb)` stops, and the `/Extend` flags.
type ShadingParams = (
    FunctionShadingType,
    Vec<f32>,
    Vec<(f32, [f32; 3])>,
    [bool; 2],
);

/// Upper bound on the number of tiled periods a repeating gradient may emit, so
/// a tiny gradient over a huge box cannot explode the stop count. Past this the
/// coverage is truncated (the remainder falls outside the clip in practice).
const MAX_GRADIENT_PERIODS: u32 = 256;

/// The four corners of `rect` as `(x, y)` pairs.
fn rect_corners(rect: Rect) -> [(f32, f32); 4] {
    [
        (rect.min_x(), rect.min_y()),
        (rect.max_x(), rect.min_y()),
        (rect.max_x(), rect.max_y()),
        (rect.min_x(), rect.max_y()),
    ]
}

/// Clamps a (possibly non-finite) period count to `0..=MAX_GRADIENT_PERIODS`.
fn clamp_periods(count: f32) -> u32 {
    if count.is_finite() && count > 0.0 {
        (count as u32).min(MAX_GRADIENT_PERIODS)
    } else {
        0
    }
}

/// How many gradient periods a linear gradient's axis must tile before (below
/// `t = 0`) and after (above `t = 1`) its base range to cover `rect`. Corners
/// are projected onto the gradient axis; the span beyond `[0, 1]` is rounded up
/// to whole periods.
fn axial_periods(a: Point, b: Point, rect: Rect) -> (u32, u32) {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len2 = dx * dx + dy * dy;
    if len2 <= f32::EPSILON {
        return (0, 0);
    }
    let mut t_min = f32::INFINITY;
    let mut t_max = f32::NEG_INFINITY;
    for (px, py) in rect_corners(rect) {
        let t = ((px - a.x) * dx + (py - a.y) * dy) / len2;
        t_min = t_min.min(t);
        t_max = t_max.max(t);
    }
    let before = clamp_periods((-t_min).ceil());
    let after = clamp_periods((t_max - 1.0).ceil());
    (before, after)
}

/// How many gradient periods a radial gradient must tile outward (past `r = rx`)
/// to cover `rect`. Corners are measured in the pre-ellipse circle space (the
/// `y` axis is unscaled by `s = ry/rx`), and the excess radius over `rx` is
/// rounded up to whole periods.
fn radial_periods(center: Point, rx: f32, s: f32, rect: Rect) -> u32 {
    let mut r_max = 0.0f32;
    for (px, py) in rect_corners(rect) {
        let dx = px - center.x;
        let dy = (py - center.y) / s;
        r_max = r_max.max((dx * dx + dy * dy).sqrt());
    }
    clamp_periods((r_max / rx - 1.0).ceil())
}

/// Tiles a single-period `[0, 1]` color ramp across `before + 1 + after` periods
/// for a repeating/reflecting gradient, returning stops over the widened `[0, 1]`
/// shading domain (the original period lands at index `before`). For `reflect`,
/// every other period is mirrored so the pattern is continuous across period
/// boundaries; for repeat the boundary is a hard edge (matching CSS). Offsets are
/// forced non-decreasing, as elsewhere.
fn tile_stops(
    base: &[(f32, [f32; 3])],
    before: u32,
    after: u32,
    reflect: bool,
) -> Vec<(f32, [f32; 3])> {
    let total = before + 1 + after;
    let p = total as f32;
    let mut out: Vec<(f32, [f32; 3])> = Vec::with_capacity(base.len() * total as usize);
    let mut last = f32::NEG_INFINITY;
    let mut push = |offset: f32, color: [f32; 3]| {
        let o = offset.clamp(0.0, 1.0).max(last);
        last = o;
        out.push((o, color));
    };
    for k in 0..total {
        let mirror = reflect && (i64::from(k) - i64::from(before)).rem_euclid(2) == 1;
        if mirror {
            for &(o, c) in base.iter().rev() {
                push((k as f32 + (1.0 - o)) / p, c);
            }
        } else {
            for &(o, c) in base {
                push((k as f32 + o) / p, c);
            }
        }
    }
    out
}

/// Builds an axial (linear) shading's parameters, honoring the gradient's extend
/// mode. `Pad` clamps the end stops over the base axis; `Repeat`/`Reflect` tile
/// the ramp across enough periods to cover `rect` (PDF shadings have no native
/// repeat), widening the axis and setting `/Extend [false false]` beyond it.
fn axial_shading(g: &LinearGradient, rect: Rect) -> ShadingParams {
    let base = norm_stops(&g.stops);
    let pad = || vec![g.start.x, g.start.y, g.end.x, g.end.y];
    if g.extend == ExtendMode::Pad {
        return (FunctionShadingType::Axial, pad(), base, [true, true]);
    }
    let (before, after) = axial_periods(g.start, g.end, rect);
    if before == 0 && after == 0 {
        // The fill lies within a single period; tiling would not be visible.
        return (FunctionShadingType::Axial, pad(), base, [true, true]);
    }
    let stops = tile_stops(&base, before, after, g.extend == ExtendMode::Reflect);
    let dx = g.end.x - g.start.x;
    let dy = g.end.y - g.start.y;
    let coords = vec![
        g.start.x - before as f32 * dx,
        g.start.y - before as f32 * dy,
        g.start.x + (1 + after) as f32 * dx,
        g.start.y + (1 + after) as f32 * dy,
    ];
    (FunctionShadingType::Axial, coords, stops, [false, false])
}

/// Builds a radial shading's parameters plus the optional y-scale transform that
/// turns the circle shading into an elliptical gradient (finding M4). Extend
/// modes are honored as in [`axial_shading`]: `Repeat`/`Reflect` tile the ramp
/// outward across enough periods to cover `rect`.
fn radial_shading(g: &RadialGradient, rect: Rect) -> (ShadingParams, Option<[f32; 6]>) {
    let base = norm_stops(&g.stops);
    let rx = g.radius.width.max(f32::EPSILON);
    let ry = g.radius.height.max(f32::EPSILON);
    let s = ry / rx;
    // Emit a circle of radius rx; scale y by ry/rx about the center to reach the
    // rx×ry ellipse (mirrors the raster backend).
    let transform =
        ((s - 1.0).abs() > f32::EPSILON).then_some([1.0, 0.0, 0.0, s, 0.0, g.center.y * (1.0 - s)]);
    let pad = || vec![g.center.x, g.center.y, 0.0, g.center.x, g.center.y, rx];
    if g.extend == ExtendMode::Pad {
        return (
            (FunctionShadingType::Radial, pad(), base, [true, true]),
            transform,
        );
    }
    let after = radial_periods(g.center, rx, s, rect);
    if after == 0 {
        return (
            (FunctionShadingType::Radial, pad(), base, [true, true]),
            transform,
        );
    }
    let stops = tile_stops(&base, 0, after, g.extend == ExtendMode::Reflect);
    let r1 = rx * (1 + after) as f32;
    let coords = vec![g.center.x, g.center.y, 0.0, g.center.x, g.center.y, r1];
    (
        (FunctionShadingType::Radial, coords, stops, [false, false]),
        transform,
    )
}

/// A [`PathSink`] over a PDF content stream. It applies a constant `(ox, oy)`
/// offset to every coordinate (zero for rects, the glyph pen origin for glyph
/// outlines) and, lacking a native quadratic, relies on [`PathSink::quad_to`]'s
/// default cubic elevation (PDF has no quadratic Bézier operator).
struct ContentSink<'a> {
    content: &'a mut Content,
    ox: f32,
    oy: f32,
}

impl PathSink for ContentSink<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.content.move_to(self.ox + x, self.oy + y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.content.line_to(self.ox + x, self.oy + y);
    }
    fn curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.content.cubic_to(
            self.ox + c1x,
            self.oy + c1y,
            self.ox + c2x,
            self.oy + c2y,
            self.ox + x,
            self.oy + y,
        );
    }
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.content.rect(self.ox + x, self.oy + y, w, h);
    }
    fn close(&mut self) {
        self.content.close_path();
    }
    /// Elevates the quadratic to a cubic in the *offset* coordinate space, so
    /// the emitted control points are bit-identical to the pre-refactor glyph
    /// walker (the default trait elevation would offset after interpolating,
    /// which floating-point non-associativity could round differently).
    fn quad_to(&mut self, cur: (f32, f32), cx: f32, cy: f32, x: f32, y: f32) {
        let (p0x, p0y) = (self.ox + cur.0, self.oy + cur.1);
        let (qx, qy) = (self.ox + cx, self.oy + cy);
        let (p1x, p1y) = (self.ox + x, self.oy + y);
        let c1 = (p0x + 2.0 / 3.0 * (qx - p0x), p0y + 2.0 / 3.0 * (qy - p0y));
        let c2 = (p1x + 2.0 / 3.0 * (qx - p1x), p1y + 2.0 / 3.0 * (qy - p1y));
        self.content.cubic_to(c1.0, c1.1, c2.0, c2.1, p1x, p1y);
    }
}

impl Builder {
    fn alloc(&mut self) -> Ref {
        let r = Ref::new(self.next_ref);
        self.next_ref += 1;
        r
    }

    fn exec(&mut self, item: &DisplayItem, resources: &ResourceTable) {
        match item {
            DisplayItem::Fill { rect, radii, brush } => self.fill(*rect, radii, brush),
            DisplayItem::Border { rect, radii, edges } => self.border(*rect, radii, edges),
            DisplayItem::Image {
                dst, image, radii, ..
            } => self.image(resources, *dst, *image, radii),
            DisplayItem::GlyphRun {
                font,
                size,
                color,
                normalized_coords,
                glyphs,
                ..
            } => self.glyph_run(resources, *font, *size, *color, normalized_coords, glyphs),
            DisplayItem::PushClip { rect, radii } => {
                self.content.save_state();
                self.rect_path(*rect, radii);
                self.content.clip_nonzero();
                self.content.end_path();
            }
            DisplayItem::PopClip => {
                self.content.restore_state();
            }
            DisplayItem::PushLayer { opacity, transform } => {
                if *opacity >= 1.0 {
                    // Opaque layer: no compositing group is needed, only the CSS
                    // transform. The page's CTM already maps CSS px (y down) to
                    // PDF user space, so the CSS matrix concatenates onto it
                    // unchanged; the matching `PopLayer` restores it.
                    self.content.save_state();
                    self.apply_layer_transform(*transform);
                    self.layers.push(LayerState::Plain);
                } else {
                    // Translucent layer: buffer its children into a form XObject
                    // (a transparency group) so overlapping children composite
                    // once at `opacity`, not per object — matching the raster
                    // backend, which draws the whole layer pixmap once.
                    let parent = std::mem::replace(&mut self.content, Content::new());
                    self.layers.push(LayerState::Group {
                        parent,
                        opacity: *opacity,
                        transform: *transform,
                    });
                }
            }
            DisplayItem::PopLayer => self.pop_layer(),
            // The PDF is painted from the document origin (unscrolled), so
            // viewport-anchor markers — which only tell a viewport render to
            // pin `position: fixed` content against the document scroll — carry
            // no geometry and are ignored.
            DisplayItem::PushViewportAnchor | DisplayItem::PopViewportAnchor => {}
        }
    }

    /// Concatenates a layer's CSS transform onto the current CTM (a no-op for
    /// the identity matrix).
    fn apply_layer_transform(&mut self, t: Transform2D) {
        if t != Transform2D::IDENTITY {
            self.content.transform([t.a, t.b, t.c, t.d, t.tx, t.ty]);
        }
    }

    /// Closes the innermost open layer. A `Plain` layer just restores the saved
    /// graphics state; a `Group` layer finalizes its buffered content into a
    /// transparency-group form XObject and draws it once under an opacity
    /// ExtGState (applying the layer transform to the invocation).
    fn pop_layer(&mut self) {
        match self.layers.pop() {
            Some(LayerState::Group {
                parent,
                opacity,
                transform,
            }) => {
                let body = std::mem::replace(&mut self.content, parent)
                    .finish()
                    .to_vec();
                let obj = self.alloc();
                let form_name = format!("Fm{}", self.forms.len()).into_bytes();
                self.forms.push(FormSpec {
                    name: form_name.clone(),
                    obj,
                    content: body,
                    bbox: [0.0, 0.0, self.doc_w, self.doc_h],
                });
                let gs_name = self.register_gstate(opacity);
                self.content.save_state();
                self.content.set_parameters(Name(&gs_name));
                self.apply_layer_transform(transform);
                self.content.x_object(Name(&form_name));
                self.content.restore_state();
            }
            // A plain (opaque) layer, or an unbalanced `PopLayer`: just restore.
            Some(LayerState::Plain) | None => {
                self.content.restore_state();
            }
        }
    }

    fn fill(&mut self, rect: Rect, radii: &BorderRadii, brush: &Brush) {
        match brush {
            Brush::Solid(color) => {
                let [r, g, b] = norm(*color);
                self.content.set_fill_rgb(r, g, b);
                self.rect_path(rect, radii);
                self.content.fill_nonzero();
            }
            Brush::LinearGradient(_) | Brush::RadialGradient(_) => {
                let (name, transform) = self.register_shading(brush, rect);
                self.content.save_state();
                self.rect_path(rect, radii);
                self.content.clip_nonzero();
                self.content.end_path();
                // An elliptical radial gradient is a circle shading under a
                // non-uniform y-scale about its center (finding M4); the clip
                // above is set first so only the shading is transformed.
                if let Some(t) = transform {
                    self.content.transform(t);
                }
                self.content.shading(Name(&name));
                self.content.restore_state();
            }
        }
    }

    fn border(&mut self, rect: Rect, radii: &BorderRadii, edges: &[BorderEdge; 4]) {
        if !edges.iter().any(BorderEdge::paints) {
            return;
        }
        // Uniform-border detection and inner geometry are shared with the raster
        // backend so the two outputs stay identical (ADR-0007 D7).
        if let Some((inner, inner_radii, edge)) = uniform_border_geometry(rect, radii, edges) {
            let [r, g, b] = norm(edge.color);
            self.content.set_fill_rgb(r, g, b);
            self.rect_path(rect, radii);
            self.rect_path(inner, &inner_radii);
            self.content.fill_even_odd();
        } else {
            // Trapezoid geometry is shared with the raster backend so the two
            // outputs stay identical (ADR-0007 D7).
            for (edge, pts) in border_edge_quads(rect, edges) {
                if !edge.paints() {
                    continue;
                }
                let [r, g, b] = norm(edge.color);
                self.content.set_fill_rgb(r, g, b);
                self.content.move_to(pts[0].0, pts[0].1);
                self.content.line_to(pts[1].0, pts[1].1);
                self.content.line_to(pts[2].0, pts[2].1);
                self.content.line_to(pts[3].0, pts[3].1);
                self.content.close_path();
                self.content.fill_nonzero();
            }
        }
    }

    fn image(
        &mut self,
        resources: &ResourceTable,
        dst: Rect,
        id: oxidepage_paint::ImageId,
        radii: &BorderRadii,
    ) {
        let Some(image) = resources.image(id) else {
            return;
        };
        // Tiling is drawn once in v1 (ADR-0007); the tile fills `dst`.
        let Some(name) = self.register_image(image, dst) else {
            return;
        };
        self.content.save_state();
        if !radii.is_zero() {
            self.rect_path(dst, radii);
            self.content.clip_nonzero();
            self.content.end_path();
        }
        // Map the unit-square image XObject onto `dst`, flipping y so the top
        // row lands at the top of `dst` in our y-down content space.
        self.content.transform([
            dst.size.width,
            0.0,
            0.0,
            -dst.size.height,
            dst.origin.x,
            dst.origin.y + dst.size.height,
        ]);
        self.content.x_object(Name(&name));
        self.content.restore_state();
    }

    fn glyph_run(
        &mut self,
        resources: &ResourceTable,
        font: FontId,
        size: f32,
        color: Color,
        coords: &[f32],
        glyphs: &[PositionedGlyph],
    ) {
        let Some(blob) = resources.font(font).cloned() else {
            return;
        };
        let data = blob.as_ref();

        // Only single-face, parseable TrueType (glyf) fonts are embedded as
        // real text. CFF/OTF, collections, and anything skrifa can't parse fall
        // back to Phase 6 vector outlines (rendered, not selectable, ADR-0008) —
        // this also ensures the embed path never references a font `write_font`
        // will fail to write. A variable-font instance (non-empty
        // `normalized_coords`) also falls back to outlines: the embedded-font
        // path shows CIDs of the default master and cannot express the
        // variation, whereas the outline path bakes the coords into the glyph
        // geometry (matching the raster backend, which varies its glyph cache).
        if !is_embeddable_truetype(data, font.index) || !coords.is_empty() {
            self.glyph_outlines(data, font.index, size, color, coords, glyphs);
            return;
        }

        let idx = self.register_font(font);
        let name = self.fonts[idx].name.clone();
        let [r, g, b] = norm(color);
        self.content.set_fill_rgb(r, g, b);
        self.content.begin_text();
        self.content.set_font(Name(&name), size);
        for glyph in glyphs {
            let Ok(gid) = u16::try_from(glyph.id) else {
                continue;
            };
            let cid = self.fonts[idx].remapper.remap(gid);
            // Position at the glyph origin. The [1,0,0,-1,ox,oy] text matrix
            // re-flips y so glyphs are upright under the page's y-flip CTM
            // (font size is applied by `set_font`).
            self.content
                .set_text_matrix([1.0, 0.0, 0.0, -1.0, glyph.x, glyph.y]);
            self.content.show(Str(&cid.to_be_bytes()));
        }
        self.content.end_text();
    }

    /// Registers a font for embedding (deduplicated by [`FontId`]), reserving
    /// its PDF object ids and resource name, and returns its index in `fonts`.
    fn register_font(&mut self, font: FontId) -> usize {
        if let Some(&idx) = self.font_index.get(&font) {
            return idx;
        }
        let type0 = self.alloc();
        let cid_font = self.alloc();
        let descriptor = self.alloc();
        let font_file = self.alloc();
        let cid_to_gid = self.alloc();
        let to_unicode = self.alloc();
        let idx = self.fonts.len();
        let name = format!("F{idx}").into_bytes();
        self.fonts.push(FontUsage {
            name,
            type0,
            cid_font,
            descriptor,
            font_file,
            cid_to_gid,
            to_unicode,
            font,
            remapper: GlyphRemapper::new(),
        });
        self.font_index.insert(font, idx);
        idx
    }

    /// Emits a glyph run as filled vector outlines (the Phase 6 path, used for
    /// non-TrueType fonts that can't be embedded as `CIDFontType2`).
    fn glyph_outlines(
        &mut self,
        data: &[u8],
        index: u32,
        size: f32,
        color: Color,
        coords: &[f32],
        glyphs: &[PositionedGlyph],
    ) {
        let [r, g, b] = norm(color);
        self.content.set_fill_rgb(r, g, b);
        let mut any = false;
        for glyph in glyphs {
            let Some(outline) = glyph_outline(data, index, glyph.id, size, coords) else {
                continue;
            };
            self.emit_glyph_path(&outline, glyph.x, glyph.y);
            any = true;
        }
        if any {
            self.content.fill_nonzero();
        }
    }

    /// Emits a glyph outline's subpaths at `(ox, oy)` via the shared
    /// [`emit_path_commands`] walker; the sink offsets each point and elevates
    /// quadratics to cubics (PDF has no quadratic Béziers).
    fn emit_glyph_path(&mut self, outline: &[PathCommand], ox: f32, oy: f32) {
        let mut sink = ContentSink {
            content: &mut self.content,
            ox,
            oy,
        };
        emit_path_commands(&mut sink, outline);
    }

    /// Emits a (possibly rounded) rectangle subpath (no fill/stroke) via the
    /// shared [`emit_rounded_rect`], keeping the outline geometry-identical with
    /// the raster backend.
    fn rect_path(&mut self, rect: Rect, radii: &BorderRadii) {
        let mut sink = ContentSink {
            content: &mut self.content,
            ox: 0.0,
            oy: 0.0,
        };
        emit_rounded_rect(&mut sink, rect, radii);
    }

    fn register_gstate(&mut self, opacity: f32) -> Vec<u8> {
        let obj = self.alloc();
        let name = format!("Gs{}", self.gstates.len()).into_bytes();
        self.gstates.push(GstateSpec {
            name: name.clone(),
            obj,
            alpha: opacity.clamp(0.0, 1.0),
        });
        name
    }

    /// Registers a gradient shading and returns its resource name plus an
    /// optional content-stream transform (the y-scale that turns a circle
    /// shading into an elliptical radial gradient, finding M4). `rect` is the
    /// fill's geometry, used to size the tiling of repeating/reflecting
    /// gradients (which PDF shadings cannot express natively).
    fn register_shading(&mut self, brush: &Brush, rect: Rect) -> (Vec<u8>, Option<[f32; 6]>) {
        let obj = self.alloc();
        let function = self.alloc();
        let name = format!("Sh{}", self.shadings.len()).into_bytes();
        let (kind, coords, stops, extend, transform) = match brush {
            Brush::LinearGradient(g) => {
                let (kind, coords, stops, extend) = axial_shading(g, rect);
                (kind, coords, stops, extend, None)
            }
            Brush::RadialGradient(g) => {
                let ((kind, coords, stops, extend), transform) = radial_shading(g, rect);
                (kind, coords, stops, extend, transform)
            }
            Brush::Solid(_) => unreachable!("solid brushes do not register shadings"),
        };
        // One exponential per adjacent stop pair; a single pair needs no
        // stitching parent (the exponential is the function directly).
        let segments = if stops.len() > 2 {
            (0..stops.len() - 1).map(|_| self.alloc()).collect()
        } else {
            Vec::new()
        };
        self.shadings.push(ShadingSpec {
            name: name.clone(),
            obj,
            function,
            segments,
            kind,
            coords,
            stops,
            extend,
        });
        (name, transform)
    }

    /// Registers an image XObject, returning its resource name. `None` when the
    /// image cannot be rasterized for embedding.
    ///
    /// Deduplication is by id *and embedded size*: a raster image has one size
    /// (its own), but a vector image is rasterized for the rect it is drawn into
    /// — the same icon at two sizes is two XObjects, and reusing the first one
    /// would be exactly the blurry upscale this path exists to avoid.
    fn register_image(&mut self, image: &Arc<DecodedImage>, dst: Rect) -> Option<Vec<u8>> {
        let (width, height) = match &image.data {
            ImageData::Raster { .. } => (image.width, image.height),
            ImageData::Vector { .. } => pdf_raster_size(dst)?,
        };
        let key = (image.id.0, width, height);
        if let Some(name) = self.image_names.get(&key) {
            return Some(name.clone());
        }

        let rgba = match &image.data {
            ImageData::Raster { rgba } => Arc::clone(rgba),
            ImageData::Vector { svg } => {
                Arc::new(oxidepage_paint::rasterize_svg(svg, width, height)?.rgba)
            }
        };
        let obj = self.alloc();
        let smask = self.alloc();
        let name = format!("Im{}", self.images.len()).into_bytes();
        self.images.push(ImageSpec {
            name: name.clone(),
            obj,
            smask,
            width,
            height,
            rgba,
        });
        self.image_names.insert(key, name.clone());
        Some(name)
    }
}

/// The raster size, in pixels, a vector image drawn into `dst` is embedded at:
/// the destination rect at [`PDF_SVG_SCALE`], clamped to the size caps. `None`
/// for a collapsed rect (nothing to embed).
fn pdf_raster_size(dst: Rect) -> Option<(u32, u32)> {
    let width = (dst.size.width * PDF_SVG_SCALE).ceil();
    let height = (dst.size.height * PDF_SVG_SCALE).ceil();
    // A NaN or non-positive extent means there is nothing to embed.
    if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
        return None;
    }
    let width = (width as u32).min(MAX_PDF_IMAGE_SIDE);
    let mut height = (height as u32).min(MAX_PDF_IMAGE_SIDE);
    if u64::from(width) * u64::from(height) > MAX_PDF_IMAGE_PIXELS {
        height = (MAX_PDF_IMAGE_PIXELS / u64::from(width)).max(1) as u32;
    }
    Some((width, height))
}
