//! The display list: a flat, backend-neutral list of paint commands produced
//! by walking the layout tree in stacking-context order (design doc §5.8).
//!
//! A [`DisplayList`] is immutable and [`Send`] once built and carries an
//! `Arc`-backed [`ResourceTable`] (font blobs, later decoded images), so
//! rasterization can run on any thread. Geometry uses the shared
//! [`oxidepage_base`] primitives; colors are 8-bit sRGBA.

use std::sync::Arc;

use oxidepage_base::{Point, Rect, Size, Transform2D};
use parley::fontique::Blob;

pub use oxidepage_layout::{DecodedImage, ImageData, ImageId};

/// Cubic-Bézier control-point ratio for approximating a quarter ellipse (the
/// rounded-corner constant shared by the raster and PDF backends so their
/// rounded rects stay geometry-identical, ADR-0007 D7).
pub const KAPPA: f32 = 0.552_284_7;

/// A straight-alpha 8-bit sRGBA color.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);
    pub const BLACK: Self = Self::rgba(0, 0, 0, 255);
    pub const WHITE: Self = Self::rgba(255, 255, 255, 255);

    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::rgba(r, g, b, 255)
    }

    /// True when fully transparent (contributes nothing when painted).
    #[must_use]
    pub const fn is_transparent(self) -> bool {
        self.a == 0
    }
}

/// The four corner radii of a box, each an (x, y) ellipse radius in the corner
/// order top-left, top-right, bottom-right, bottom-left.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct BorderRadii {
    pub top_left: Size,
    pub top_right: Size,
    pub bottom_right: Size,
    pub bottom_left: Size,
}

impl BorderRadii {
    pub const ZERO: Self = Self {
        top_left: Size::ZERO,
        top_right: Size::ZERO,
        bottom_right: Size::ZERO,
        bottom_left: Size::ZERO,
    };

    /// Uniform radius on all corners.
    #[must_use]
    pub fn uniform(radius: f32) -> Self {
        let s = Size::new(radius, radius);
        Self {
            top_left: s,
            top_right: s,
            bottom_right: s,
            bottom_left: s,
        }
    }

    /// True when every corner radius is zero (a plain rectangle).
    #[must_use]
    pub fn is_zero(&self) -> bool {
        [
            self.top_left,
            self.top_right,
            self.bottom_right,
            self.bottom_left,
        ]
        .iter()
        .all(|s| s.width <= 0.0 && s.height <= 0.0)
    }

    /// Clamps the radii so adjacent corners never overlap, per CSS
    /// Backgrounds §5.2: if the radii along any edge sum to more than the
    /// edge length, all radii are scaled down by the smallest ratio `f`.
    #[must_use]
    pub fn clamped_to(&self, size: Size) -> Self {
        let mut f = 1.0f32;
        let mut consider = |sum: f32, extent: f32| {
            if sum > 0.0 {
                f = f.min(extent / sum);
            }
        };
        consider(self.top_left.width + self.top_right.width, size.width); // top edge
        consider(self.bottom_left.width + self.bottom_right.width, size.width); // bottom edge
        consider(self.top_left.height + self.bottom_left.height, size.height); // left edge
        consider(
            self.top_right.height + self.bottom_right.height,
            size.height,
        ); // right edge

        let scale = |s: Size| {
            if f < 1.0 {
                Size::new((s.width * f).max(0.0), (s.height * f).max(0.0))
            } else {
                Size::new(s.width.max(0.0), s.height.max(0.0))
            }
        };
        Self {
            top_left: scale(self.top_left),
            top_right: scale(self.top_right),
            bottom_right: scale(self.bottom_right),
            bottom_left: scale(self.bottom_left),
        }
    }

    /// Shrinks every corner radius inward by a uniform border width `w`
    /// (clamped at zero), for the inner ring of a uniform border. Shared by
    /// the raster and PDF backends (ADR-0007 D7).
    #[must_use]
    pub fn shrunk_by(&self, w: f32) -> Self {
        let shrink = |s: Size| Size::new((s.width - w).max(0.0), (s.height - w).max(0.0));
        Self {
            top_left: shrink(self.top_left),
            top_right: shrink(self.top_right),
            bottom_right: shrink(self.bottom_right),
            bottom_left: shrink(self.bottom_left),
        }
    }
}

/// One color stop of a gradient, at a normalized position in `[0, 1]`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct GradientStop {
    pub offset: f32,
    pub color: Color,
}

/// How a gradient (or tiled image) extends beyond its defined range.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ExtendMode {
    /// Clamp the end stops (the CSS default).
    #[default]
    Pad,
    /// Repeat the stop sequence (`repeating-*-gradient`).
    Repeat,
    /// Mirror the stop sequence.
    Reflect,
}

/// A two-point linear gradient in display-list coordinates.
#[derive(Clone, PartialEq, Debug)]
pub struct LinearGradient {
    pub start: Point,
    pub end: Point,
    pub stops: Vec<GradientStop>,
    pub extend: ExtendMode,
}

/// An axis-aligned elliptical radial gradient.
#[derive(Clone, PartialEq, Debug)]
pub struct RadialGradient {
    pub center: Point,
    /// Horizontal and vertical radius of the ending shape.
    pub radius: Size,
    pub stops: Vec<GradientStop>,
    pub extend: ExtendMode,
}

/// A paint source for a [`DisplayItem::Fill`].
#[derive(Clone, PartialEq, Debug)]
pub enum Brush {
    Solid(Color),
    LinearGradient(LinearGradient),
    RadialGradient(RadialGradient),
}

/// Identifies a font face by its resource blob id and collection index.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FontId {
    /// [`parley::fontique::Blob::id`] of the font data.
    pub blob: u64,
    /// Face index within a font collection (`.ttc`).
    pub index: u32,
}

/// A shaped glyph positioned in display-list coordinates (its origin is the
/// glyph's pen position on the run baseline).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PositionedGlyph {
    pub id: u32,
    pub x: f32,
    pub y: f32,
}

/// How an image fills its destination rect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TileMode {
    /// Draw once, scaled to the destination (`<img>` and no-repeat layers).
    Stretch,
    /// Tile on both axes across the current clip; `dst` is one tile.
    Repeat,
    /// Tile horizontally only.
    RepeatX,
    /// Tile vertically only.
    RepeatY,
}

/// A CSS border-line style. Every style except `none`/`hidden` is rasterized
/// as `solid` in v1 (ADR-0007 D7).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BorderStyle {
    #[default]
    None,
    Hidden,
    Solid,
    Double,
    Dotted,
    Dashed,
    Groove,
    Ridge,
    Inset,
    Outset,
}

impl BorderStyle {
    /// True when this style produces visible paint.
    #[must_use]
    pub fn paints(self) -> bool {
        !matches!(self, BorderStyle::None | BorderStyle::Hidden)
    }
}

/// One edge of a border (top, right, bottom, left order in
/// [`DisplayItem::Border`]).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct BorderEdge {
    pub width: f32,
    pub color: Color,
    pub style: BorderStyle,
}

impl BorderEdge {
    /// True when the edge contributes visible paint.
    #[must_use]
    pub fn paints(&self) -> bool {
        self.width > 0.0 && self.style.paints() && !self.color.is_transparent()
    }
}

/// The four border-edge trapezoids of a border box (top, right, bottom, left),
/// each as its [`BorderEdge`] paired with an outer→inner quad in the order
/// (outer-start, outer-end, inner-end, inner-start). Corners are mitered
/// (radii ignored). Shared by the raster and PDF backends so their non-uniform
/// border geometry stays identical (ADR-0007 D7).
#[must_use]
pub fn border_edge_quads(
    rect: Rect,
    edges: &[BorderEdge; 4],
) -> [(BorderEdge, [(f32, f32); 4]); 4] {
    let (x, y) = (rect.origin.x, rect.origin.y);
    let (w, h) = (rect.size.width, rect.size.height);
    let (t, r, b, l) = (
        edges[0].width,
        edges[1].width,
        edges[2].width,
        edges[3].width,
    );
    [
        (
            edges[0],
            [(x, y), (x + w, y), (x + w - r, y + t), (x + l, y + t)],
        ),
        (
            edges[1],
            [
                (x + w, y),
                (x + w, y + h),
                (x + w - r, y + h - b),
                (x + w - r, y + t),
            ],
        ),
        (
            edges[2],
            [
                (x + w, y + h),
                (x, y + h),
                (x + l, y + h - b),
                (x + w - r, y + h - b),
            ],
        ),
        (
            edges[3],
            [(x, y + h), (x, y), (x + l, y + t), (x + l, y + h - b)],
        ),
    ]
}

/// When every edge of a border paints with the same width and color, returns
/// the inner (padding-box) rect, its inward-shrunk radii, and the shared edge —
/// the uniform border can then be drawn as a single even-odd ring around a hole.
/// Returns `None` for a non-uniform border (drawn per-edge via
/// [`border_edge_quads`]). Shared by the raster and PDF backends so their
/// uniform-border geometry stays identical (ADR-0007 D7).
#[must_use]
pub fn uniform_border_geometry(
    rect: Rect,
    radii: &BorderRadii,
    edges: &[BorderEdge; 4],
) -> Option<(Rect, BorderRadii, BorderEdge)> {
    let uniform = edges.iter().all(BorderEdge::paints)
        && edges
            .iter()
            .all(|e| e.width == edges[0].width && e.color == edges[0].color);
    if !uniform {
        return None;
    }
    let w = edges[0].width;
    let inner = Rect::from_xywh(
        rect.origin.x + w,
        rect.origin.y + w,
        (rect.size.width - 2.0 * w).max(0.0),
        (rect.size.height - 2.0 * w).max(0.0),
    );
    Some((inner, radii.shrunk_by(w), edges[0]))
}

/// A single paint command. Built by walking the layout tree in stacking-context
/// paint order (design doc §5.8).
#[derive(Clone, PartialEq, Debug)]
pub enum DisplayItem {
    /// Fill a (possibly rounded) rect with a solid color or gradient.
    Fill {
        rect: Rect,
        radii: BorderRadii,
        brush: Brush,
    },
    /// Stroke the four edges of a (possibly rounded) border box.
    Border {
        rect: Rect,
        radii: BorderRadii,
        /// Edges in top, right, bottom, left order.
        edges: [BorderEdge; 4],
    },
    /// Draw a decoded image into `dst` (stretched or tiled).
    Image {
        dst: Rect,
        image: ImageId,
        tile: TileMode,
        radii: BorderRadii,
    },
    /// A run of shaped glyphs sharing one font, size, and color.
    GlyphRun {
        font: FontId,
        size: f32,
        color: Color,
        /// Normalized variable-font coordinates (empty for static fonts).
        normalized_coords: Vec<f32>,
        glyphs: Vec<PositionedGlyph>,
        /// Source text for golden review; ignored by rasterizers.
        debug_text: Option<String>,
    },
    /// Push a rounded-rect clip (paired with [`DisplayItem::PopClip`]).
    PushClip {
        rect: Rect,
        radii: BorderRadii,
    },
    PopClip,
    /// Push an opacity/transform group (paired with [`DisplayItem::PopLayer`]).
    /// `transform` carries the element's CSS transform (ADR-0013 D2); backends
    /// must apply it to every item inside the layer, not assume identity.
    PushLayer {
        opacity: f32,
        transform: Transform2D,
    },
    PopLayer,
    /// Marks the start of a viewport-anchored (`position: fixed`) subtree. A
    /// viewport render suppresses the document scroll offset it applies to all
    /// other content until the matching [`DisplayItem::PopViewportAnchor`], so
    /// the subtree stays pinned to the viewport while document content scrolls
    /// under it. A no-op for backends that paint the document unscrolled
    /// (full-page raster and PDF).
    PushViewportAnchor,
    PopViewportAnchor,
}

/// A font face referenced by the display list: its [`FontId`] plus the shared
/// font data blob the rasterizer builds a [`skrifa`](https://docs.rs/skrifa)
/// `FontRef` from.
#[derive(Clone)]
pub struct FontResource {
    pub id: FontId,
    pub data: Blob<u8>,
}

impl std::fmt::Debug for FontResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontResource")
            .field("id", &self.id)
            .field("len", &self.data.len())
            .finish()
    }
}

/// Shared, `Send` resource table carried alongside the display list: font
/// blobs (glyph rasterization) and decoded images (`<img>` / background
/// `url()`).
#[derive(Clone, Debug, Default)]
pub struct ResourceTable {
    pub fonts: Vec<FontResource>,
    pub images: Vec<Arc<DecodedImage>>,
}

impl ResourceTable {
    /// Records `resource` if its [`FontId`] is not already present.
    pub fn add_font(&mut self, resource: FontResource) {
        if !self.fonts.iter().any(|f| f.id == resource.id) {
            self.fonts.push(resource);
        }
    }

    /// The font data blob for `id`, if registered.
    #[must_use]
    pub fn font(&self, id: FontId) -> Option<&Blob<u8>> {
        self.fonts.iter().find(|f| f.id == id).map(|f| &f.data)
    }

    /// Folds another list's resources in, skipping what is already present.
    ///
    /// No id rebasing, and that is a property of the design rather than luck: a
    /// `FontId` is a content hash, and every browsing context of a page shares
    /// one `ImageStore` and therefore one `ImageId` space (ADR-0035 D7). Give
    /// frames their own stores again and this becomes silently wrong pictures.
    pub fn merge(&mut self, other: &Self) {
        for font in &other.fonts {
            self.add_font(font.clone());
        }
        for image in &other.images {
            self.add_image(Arc::clone(image));
        }
    }

    /// Records a decoded image if its [`ImageId`] is not already present.
    pub fn add_image(&mut self, image: Arc<DecodedImage>) {
        if !self.images.iter().any(|i| i.id == image.id) {
            self.images.push(image);
        }
    }

    /// The decoded image for `id`, if registered.
    #[must_use]
    pub fn image(&self, id: ImageId) -> Option<&Arc<DecodedImage>> {
        self.images.iter().find(|i| i.id == id)
    }
}

/// A complete display list for one document paint.
#[derive(Clone, Debug)]
pub struct DisplayList {
    /// The paint viewport (CSS px).
    pub viewport: Size,
    /// The document's scrollable content extent (CSS px).
    pub content_size: Size,
    pub items: Vec<DisplayItem>,
    pub resources: ResourceTable,
}

impl DisplayList {
    /// An empty list for `viewport`.
    #[must_use]
    pub fn empty(viewport: Size) -> Self {
        Self {
            viewport,
            content_size: viewport,
            items: Vec::new(),
            resources: ResourceTable::default(),
        }
    }

    /// A stable, human-reviewable JSON dump for `dump --format display-list` and
    /// golden tests (floats fixed to two decimals; see [`crate::json`]).
    #[must_use]
    pub fn to_json(&self) -> String {
        crate::json::display_list_to_json(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_list_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<DisplayList>();
        assert_send::<ResourceTable>();
        assert_send::<DisplayItem>();
    }

    #[test]
    fn radii_clamp_overlap() {
        // A 100×100 box with uniform radius 60: each edge sums to 120 > 100,
        // so f = 100/120 and every radius scales to 50.
        let radii = BorderRadii::uniform(60.0);
        let clamped = radii.clamped_to(Size::new(100.0, 100.0));
        for corner in [
            clamped.top_left,
            clamped.top_right,
            clamped.bottom_right,
            clamped.bottom_left,
        ] {
            assert!((corner.width - 50.0).abs() < 1e-3, "got {}", corner.width);
            assert!((corner.height - 50.0).abs() < 1e-3, "got {}", corner.height);
        }

        // Fitting radii are left untouched.
        let small = BorderRadii::uniform(10.0);
        let unchanged = small.clamped_to(Size::new(100.0, 100.0));
        assert_eq!(unchanged.top_left, Size::new(10.0, 10.0));
    }
}
