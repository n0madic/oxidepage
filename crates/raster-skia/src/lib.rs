//! DisplayList → tiny-skia CPU raster → RGBA / PNG (design doc §5.9, Phase 6).
//!
//! The interpreter walks a [`DisplayList`] onto a `tiny_skia::Pixmap`,
//! honoring clip and opacity-layer stacks. Output is deterministic per
//! platform for a fixed font set (the reftest requirement). Glyph runs are
//! rasterized in WP-G; image items in WP-L.

pub(crate) mod canvas;
pub(crate) mod glyphs;
pub(crate) mod path;

use oxidepage_base::{Point, Size};
use oxidepage_paint::{Color, DisplayList};

/// A rendered RGBA image (straight alpha, 8 bits per channel, row-major).
#[derive(Clone)]
pub struct RasterImage {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, RGBA.
    pub rgba: Vec<u8>,
}

impl RasterImage {
    /// The RGBA pixel at `(x, y)` (out-of-bounds → transparent black).
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        if x >= self.width || y >= self.height {
            return [0, 0, 0, 0];
        }
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[i],
            self.rgba[i + 1],
            self.rgba[i + 2],
            self.rgba[i + 3],
        ]
    }
}

/// Rasterization options.
#[derive(Clone, Copy, Debug)]
pub struct RasterOptions {
    /// Device pixel ratio; the output is `ceil(viewport * scale)` px.
    pub scale: f32,
    /// The base (canvas) color painted before the display list.
    pub background: Color,
}

impl Default for RasterOptions {
    fn default() -> Self {
        Self {
            scale: 1.0,
            background: Color::WHITE,
        }
    }
}

/// Largest device dimension (px) on either axis, and the largest device area.
/// The output size is `viewport × dpr`, both embedder-controlled; an absurd
/// pairing would allocate (and per-layer re-allocate) gigabytes. These bound
/// the base pixmap, every clip mask, and every opacity-layer surface (L3, H1).
const MAX_DEVICE_SIDE: u32 = 16_384;
const MAX_DEVICE_AREA: u64 = 64_000_000;

/// Clamps a requested device size to the per-side and total-area caps. Content
/// beyond the clamped extent is simply not rasterized (graceful degradation for
/// an absurd viewport × dpr) rather than triggering a huge allocation.
pub(crate) fn clamp_device_size(width: f32, height: f32) -> (u32, u32) {
    let width = (width as u32).clamp(1, MAX_DEVICE_SIDE);
    let mut height = (height as u32).clamp(1, MAX_DEVICE_SIDE);
    if u64::from(width) * u64::from(height) > MAX_DEVICE_AREA {
        let max_h = (MAX_DEVICE_AREA / u64::from(width)).min(u64::from(MAX_DEVICE_SIDE));
        height = (max_h as u32).max(1);
    }
    (width, height)
}

/// Rasterizes `list` to an RGBA image sized to the paint viewport, at document
/// scroll `(0, 0)`. See [`render_scrolled`] to render a scrolled viewport.
#[must_use]
pub fn render(list: &DisplayList, options: &RasterOptions) -> RasterImage {
    render_scrolled(list, options, Point::ZERO)
}

/// Rasterizes `list` to the viewport with the document scrolled by `scroll`
/// (CSS px). The list is built unscrolled (one list per layout, cached across
/// scroll positions), so the document scroll is applied here: content is
/// translated by `-scroll`, except viewport-anchored (`position: fixed`)
/// subtrees — delimited by [`oxidepage_paint::DisplayItem::PushViewportAnchor`]
/// — which stay pinned. Content is clipped to the viewport.
#[must_use]
pub fn render_scrolled(list: &DisplayList, options: &RasterOptions, scroll: Point) -> RasterImage {
    render_sized(list, options, list.viewport, scroll)
}

/// Rasterizes `list` over the whole document extent (`content_size`) instead of
/// the viewport, for full-page screenshots.
///
/// `list` must have been built by `build_display_list_full`, so it is painted
/// from the document's top-left and ignores the viewport (document) scroll —
/// the same geometry the PDF export uses (ADR-0007 D8).
#[must_use]
pub fn render_full_page(list: &DisplayList, options: &RasterOptions) -> RasterImage {
    // The whole document is painted from its top-left, so document scroll does
    // not apply; viewport-anchor markers become no-ops at zero scroll.
    render_sized(list, options, list.content_size, Point::ZERO)
}

fn render_sized(
    list: &DisplayList,
    options: &RasterOptions,
    size: Size,
    scroll: Point,
) -> RasterImage {
    let scale = options.scale.max(f32::EPSILON);
    let (width, height) = clamp_device_size(
        (size.width * scale).ceil().max(1.0),
        (size.height * scale).ceil().max(1.0),
    );

    // A `None` here means even the clamped base pixmap could not be allocated
    // (genuine OOM); return a tiny transparent placeholder instead of panicking.
    let Some(mut canvas) = canvas::Canvas::new(width, height, scale, options.background, scroll)
    else {
        return RasterImage {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 0],
        };
    };
    canvas.run(list);
    let pixmap = canvas.finish();

    RasterImage {
        width,
        height,
        rgba: pixmap.take_demultiplied(),
    }
}

/// Encodes a [`RasterImage`] as a PNG byte stream.
///
/// # Errors
/// Returns any error from the `png` encoder (writing into the in-memory
/// buffer does not fail in practice).
pub fn encode_png(image: &RasterImage) -> Result<Vec<u8>, png::EncodingError> {
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&image.rgba)?;
    }
    Ok(buf)
}
