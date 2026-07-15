//! Image-decode hardening: header-declared dimensions must be bounded so a
//! hostile image cannot force a gigabyte allocation (M2 SVG, M3 raster).

use oxidepage_paint::{DecodedImageData, decode_image};

/// The raster pixels of a decode, or a failure if it came back vector.
fn raster(data: DecodedImageData) -> oxidepage_paint::DecodedPixels {
    match data {
        DecodedImageData::Raster(pixels) => pixels,
        DecodedImageData::Vector(_) => panic!("expected a raster image, got a vector one"),
    }
}

/// The intrinsic size and markup of a decode, or a failure if it came back
/// raster.
#[cfg(feature = "svg")]
fn vector(data: DecodedImageData) -> oxidepage_paint::VectorImage {
    match data {
        DecodedImageData::Vector(image) => image,
        DecodedImageData::Raster(_) => panic!("expected a vector image, got a raster one"),
    }
}

/// CRC-32 (IEEE, reflected) — PNG chunk checksum.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Appends a PNG chunk (length + type + data + CRC).
fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// A structurally valid PNG whose IHDR declares `w × h` (RGBA). The IDAT is a
/// dummy chunk: the decoder reads the header, checks dimensions against its
/// limits, and rejects before ever decompressing the pixel data.
fn png_header_declaring(w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // color type: RGBA
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(
        &mut out,
        b"IDAT",
        &[0x78, 0x9c, 0x63, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01],
    );
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A canonical 1×1 transparent PNG (valid, decodable) — the control.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

#[test]
fn small_png_decodes() {
    // Control: the limits do not reject a legitimately small image.
    let pixels = raster(decode_image(TINY_PNG, Some("image/png")).expect("tiny PNG decodes"));
    assert_eq!((pixels.width, pixels.height), (1, 1));
}

#[test]
fn huge_declared_png_is_rejected_by_the_limits() {
    // 100000 × 100000 RGBA is ≈37 GiB. It must be refused from the header, not
    // decoded (the `image` default has no width/height cap).
    let bytes = png_header_declaring(100_000, 100_000);
    assert!(
        decode_image(&bytes, Some("image/png")).is_none(),
        "over-large declared dimensions must be rejected before allocation"
    );
}

#[cfg(feature = "svg")]
#[test]
fn huge_declared_svg_is_rejected_by_the_dimension_cap() {
    // The intrinsic size comes from the SVG's own width/height attributes; a
    // 100000 × 100000 canvas would size a multi-gigabyte pixmap.
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="100000"></svg>"#;
    assert!(
        decode_image(svg, Some("image/svg+xml")).is_none(),
        "over-large SVG intrinsic size must be rejected before allocation"
    );
}

#[cfg(feature = "svg")]
#[test]
fn small_svg_decodes_to_markup_and_an_intrinsic_size() {
    // An SVG is not rasterized at decode time: it is parsed for the intrinsic
    // size layout needs, and the markup is kept for the backend to rasterize at
    // whatever size the element paints at.
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"></svg>"#;
    let image = vector(decode_image(svg, Some("image/svg+xml")).expect("small SVG decodes"));
    assert_eq!((image.width, image.height), (8, 8));
    assert_eq!(image.svg, svg, "the source markup is kept verbatim");
}

#[cfg(feature = "svg")]
#[test]
fn rasterize_svg_renders_at_the_requested_size() {
    // The backend asks for the device size it will blit, not the intrinsic one:
    // an 8×8 icon shown at 64×64 is rendered at 64×64, sharp, rather than being
    // upscaled from 8×8 pixels.
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="red"/></svg>"#;
    let pixels = oxidepage_paint::rasterize_svg(svg, 64, 64).expect("SVG rasterizes");
    assert_eq!((pixels.width, pixels.height), (64, 64));
    assert_eq!(pixels.rgba.len(), 64 * 64 * 4);
    assert!(
        pixels.rgba.chunks_exact(4).all(|px| px == [255, 0, 0, 255]),
        "the whole canvas is the scaled-up rect, not a padded 8×8 blit"
    );
}

#[cfg(feature = "svg")]
#[test]
fn rasterize_svg_rejects_an_over_large_request() {
    // The device size is derived from a page-controlled rect, so it gets the same
    // caps as a declared intrinsic size.
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"></svg>"#;
    assert!(oxidepage_paint::rasterize_svg(svg, 100_000, 100_000).is_none());
}

/// An `<image href>` in an untrusted SVG must never reach the filesystem: usvg's
/// default string resolver opens the href as a path. A referenced *SVG* file is
/// then rendered straight into the output (`ImageKind::SVG` needs no raster
/// feature), disclosing it. Inline `data:` images must keep working.
///
/// The href is resolved when the tree is parsed, and the backend re-parses to
/// rasterize — so these go through `rasterize_svg`, the path that now produces
/// every SVG pixel the engine paints.
#[cfg(feature = "svg")]
mod svg_image_href {
    use oxidepage_paint::rasterize_svg;

    /// An opaque red canvas — the bait, and the thing we look for in the output.
    const RED_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="red"/></svg>"#;

    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for group in bytes.chunks(3) {
            let padded = [
                group[0],
                *group.get(1).unwrap_or(&0),
                *group.get(2).unwrap_or(&0),
            ];
            let bits =
                (u32::from(padded[0]) << 16) | (u32::from(padded[1]) << 8) | u32::from(padded[2]);
            for i in 0..4 {
                if i <= group.len() {
                    out.push(char::from(ALPHABET[(bits >> (18 - 6 * i)) as usize & 0x3f]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// Wraps `href` in an 8×8 SVG that draws it over the whole canvas.
    fn svg_embedding(href: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="8" height="8"><image xlink:href="{href}" width="8" height="8"/></svg>"#
        )
    }

    /// True when any pixel is opaque — i.e. the referenced image was composited.
    fn painted(bytes: &[u8]) -> bool {
        let pixels = rasterize_svg(bytes, 8, 8).expect("SVG rasterizes");
        pixels.rgba.chunks_exact(4).any(|px| px[3] != 0)
    }

    #[test]
    fn control_rect_renders() {
        // Proves the SVG pipeline paints at all, so "nothing painted" below is a
        // real signal rather than a broken renderer.
        assert!(painted(RED_SVG.as_bytes()), "the rect should be painted");
    }

    #[test]
    fn data_url_image_still_renders() {
        // `resolve_data` is deliberately left at its default, so inline images work.
        let href = format!("data:image/svg+xml;base64,{}", base64(RED_SVG.as_bytes()));
        assert!(
            painted(svg_embedding(&href).as_bytes()),
            "an inline data: image must still be composited"
        );
    }

    #[test]
    fn local_file_href_is_not_read() {
        // A real, readable SVG on disk. usvg's default resolver opens it by absolute
        // path and resvg renders it; ours must refuse and leave the canvas blank.
        let path = std::env::temp_dir().join(format!(
            "oxidepage-svg-href-{}-{:?}.svg",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, RED_SVG).expect("write the bait file");

        let leaked = painted(svg_embedding(&path.display().to_string()).as_bytes());

        std::fs::remove_file(&path).ok();
        assert!(
            !leaked,
            "an <image href> pointing at a local file must not be read or composited"
        );
    }
}
