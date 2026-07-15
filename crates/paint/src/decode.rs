//! Image decoding (ADR-0007 D8, WP-K). Raster formats go through the `image`
//! crate (PNG/JPEG/GIF always; WebP behind the `webp` feature) and decode to
//! pixels here. SVG (behind the `svg` feature) is *not* rasterized here: it is
//! only parsed, for its intrinsic size, and kept as markup — the backends
//! rasterize it at the final device size via [`rasterize_svg`], so an icon shown
//! at 10× its `viewBox` stays sharp (ADR-0013 D5). Decoding runs synchronously
//! on the page thread (a deviation from the design's decode pool; ADR-0007).

/// Largest accepted image dimension (px) on either axis. A hostile image can
/// declare an enormous width/height in its header; decoding it would allocate
/// gigabytes. Anything larger is rejected before allocation.
const MAX_IMAGE_SIDE: u32 = 16_384;

/// Largest accepted image area (px). Bounds `width × height` independently of
/// the per-side cap so a `16384 × 16384` (≈1 GiB RGBA) image is still rejected.
const MAX_IMAGE_PIXELS: u64 = 40_000_000;

/// Largest buffer the decoder may allocate while decoding. Tighter than the
/// `image` crate's 512 MiB default; combined with the dimension caps it stops a
/// single header-declared image from exhausting memory.
const MAX_IMAGE_ALLOC: u64 = 256 * 1024 * 1024;

/// The RGBA pixels and dimensions of a decoded image (straight alpha,
/// row-major, `width * height * 4` bytes).
pub struct DecodedPixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A vector image: the source markup plus the intrinsic size layout sizes the
/// replaced element from. The markup is rasterized by the backend, at the size
/// the element actually paints at.
pub struct VectorImage {
    pub width: u32,
    pub height: u32,
    pub svg: Vec<u8>,
}

/// The outcome of decoding image bytes: pixels for a raster format, markup plus
/// an intrinsic size for a vector one.
pub enum DecodedImageData {
    Raster(DecodedPixels),
    Vector(VectorImage),
}

/// True when `width`/`height` are within the accepted per-side and total-area
/// caps (and non-zero). Shared by the raster and SVG paths.
fn dimensions_ok(width: u32, height: u32) -> bool {
    width != 0
        && height != 0
        && width <= MAX_IMAGE_SIDE
        && height <= MAX_IMAGE_SIDE
        && u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS
}

/// Decodes `bytes`: a raster image to RGBA, or (when the `svg` feature is on) an
/// SVG to its markup plus intrinsic size. Returns `None` on an
/// unsupported/corrupt/over-large image.
#[must_use]
pub fn decode_image(bytes: &[u8], content_type: Option<&str>) -> Option<DecodedImageData> {
    #[cfg(feature = "svg")]
    if content_type.is_some_and(is_svg_type) || looks_like_svg(bytes) {
        return parse_svg(bytes).map(DecodedImageData::Vector);
    }
    let _ = content_type;

    // Decode through an explicit-limit reader: the `image` default applies a
    // 512 MiB alloc cap but NO width/height cap, so a header declaring huge
    // dimensions is otherwise accepted and materialized. The dimension caps are
    // checked against the header before pixel data is allocated.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);

    // Probe the header first: `image`'s limits cover the per-side caps and the
    // decoder's own allocations, but not the total area, and `to_rgba8` expands
    // to 4 bytes/px afterwards. Checking the declared area up front keeps an
    // 8000×8000 JPEG (legal per-side, ≈256 MB as RGBA) from being fully decoded
    // and expanded only to be discarded.
    let mut probe = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    probe.limits(limits.clone());
    let (width, height) = probe.into_dimensions().ok()?;
    if !dimensions_ok(width, height) {
        return None;
    }

    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let image = reader.decode().ok()?;

    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    if !dimensions_ok(width, height) {
        return None;
    }
    Some(DecodedImageData::Raster(DecodedPixels {
        width,
        height,
        rgba: rgba.into_raw(),
    }))
}

#[cfg(feature = "svg")]
fn is_svg_type(content_type: &str) -> bool {
    content_type.to_ascii_lowercase().contains("svg")
}

#[cfg(feature = "svg")]
fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(256)];
    let text = String::from_utf8_lossy(head);
    text.contains("<svg") || text.contains("<?xml")
}

/// Parse options for untrusted SVG content.
///
/// usvg's default `resolve_string` hook treats `<image href="...">` as a
/// filesystem path and reads it. The SVG bytes here come from page content, so
/// the default would let a hostile document composite an arbitrary local file
/// into the render and disclose it to whoever receives the output. Refusing every
/// non-data href closes that; `resolve_data` is left at its default so inline
/// `data:` images keep working. `resources_dir` stays `None` for the same reason.
#[cfg(feature = "svg")]
fn svg_options() -> resvg::usvg::Options<'static> {
    use resvg::usvg;

    let mut options = usvg::Options {
        resources_dir: None,
        ..usvg::Options::default()
    };
    options.image_href_resolver.resolve_string = Box::new(|_href, _options| None);
    options
}

/// Parses an SVG for its intrinsic size, keeping the markup for the backends to
/// rasterize. No pixels are produced here — see [`rasterize_svg`].
#[cfg(feature = "svg")]
fn parse_svg(bytes: &[u8]) -> Option<VectorImage> {
    use resvg::usvg;

    let tree = usvg::Tree::from_data(bytes, &svg_options()).ok()?;
    let size = tree.size().to_int_size();
    let (width, height) = (size.width(), size.height());
    // The intrinsic size comes straight from the (untrusted) SVG's declared
    // width/height, and it is what layout sizes the replaced box from; a
    // `width="100000" height="100000"` would drive a multi-gigabyte
    // rasterization downstream. Reject it here, once.
    if !dimensions_ok(width, height) {
        return None;
    }
    Some(VectorImage {
        width,
        height,
        svg: bytes.to_vec(),
    })
}

/// Rasterizes SVG markup to exactly `width × height` device pixels.
///
/// This is the backends' entry point: they know the final device size (the
/// destination rect through the CTM, including the device pixel ratio and any
/// CSS `transform`), so the SVG is rendered at that resolution rather than
/// scaled up from its intrinsic size. Returns `None` on a corrupt SVG or an
/// over-large request; without the `svg` feature, always `None`.
#[must_use]
pub fn rasterize_svg(svg: &[u8], width: u32, height: u32) -> Option<DecodedPixels> {
    #[cfg(not(feature = "svg"))]
    {
        let _ = (svg, width, height);
        None
    }

    #[cfg(feature = "svg")]
    {
        use resvg::tiny_skia;
        use resvg::usvg;

        if !dimensions_ok(width, height) {
            return None;
        }
        let tree = usvg::Tree::from_data(svg, &svg_options()).ok()?;
        let size = tree.size();
        if size.width() <= 0.0 || size.height() <= 0.0 {
            return None;
        }
        let transform = tiny_skia::Transform::from_scale(
            width as f32 / size.width(),
            height as f32 / size.height(),
        );
        let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        Some(DecodedPixels {
            width,
            height,
            rgba: pixmap.take_demultiplied(),
        })
    }
}
