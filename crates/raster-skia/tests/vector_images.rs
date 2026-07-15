//! A vector image is rasterized at the size it actually paints at, not upscaled
//! from its intrinsic size (ADR-0013 D5).
//!
//! The proof is edge sharpness. A `viewBox="0 0 2 2"` SVG stored as 2×2 *pixels*
//! and blitted into a 200×200 box interpolates the two texel centers across the
//! whole box: the boundary becomes a 100px-wide gradient. Rasterized at 200×200
//! it is one hard edge. So "the pixels either side of the boundary are pure, with
//! no intermediate values" is exactly the property the old code could not have.

use std::sync::Arc;

use oxidepage_base::{Rect, Size};
use oxidepage_paint::{
    BorderRadii, DecodedImage, DisplayItem, DisplayList, ImageData, ImageId, ResourceTable,
    TileMode,
};
use oxidepage_raster_skia::{RasterOptions, render};

/// A 2×2 SVG: left half black, right half green, with the boundary exactly down
/// the middle of the viewBox.
const HALVES_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2" viewBox="0 0 2 2"><rect x="0" y="0" width="1" height="2" fill="#000000"/><rect x="1" y="0" width="1" height="2" fill="#00ff00"/></svg>"##;

fn halves_image() -> Arc<DecodedImage> {
    Arc::new(DecodedImage {
        id: ImageId(1),
        width: 2,
        height: 2,
        data: ImageData::Vector {
            svg: Arc::new(HALVES_SVG.to_vec()),
        },
    })
}

/// A display list drawing the vector image into `dst`, on a `viewport`-sized
/// canvas.
fn list(viewport: f32, dst: Rect) -> DisplayList {
    DisplayList {
        viewport: Size::new(viewport, viewport),
        content_size: Size::new(viewport, viewport),
        items: vec![DisplayItem::Image {
            dst,
            image: ImageId(1),
            tile: TileMode::Stretch,
            radii: BorderRadii::ZERO,
        }],
        resources: ResourceTable {
            images: vec![halves_image()],
            ..ResourceTable::default()
        },
    }
}

/// Asserts the halves are drawn with a hard edge at device column `boundary`:
/// pure black just left of it, pure green just right, no interpolated pixels.
fn assert_sharp_boundary(img: &oxidepage_raster_skia::RasterImage, boundary: u32, row: u32) {
    let left = img.pixel(boundary - 2, row);
    let right = img.pixel(boundary + 2, row);
    assert_eq!(
        left,
        [0, 0, 0, 255],
        "just left of the boundary must be pure black, not a blend; got {left:?}"
    );
    assert_eq!(
        right,
        [0, 255, 0, 255],
        "just right of the boundary must be pure green, not a blend; got {right:?}"
    );
}

#[test]
fn vector_image_is_sharp_when_scaled_far_above_its_intrinsic_size() {
    // A 2×2 icon shown at 200×200 — a 100× upscale.
    let img = render(
        &list(200.0, Rect::from_xywh(0.0, 0.0, 200.0, 200.0)),
        &RasterOptions::default(),
    );
    assert_sharp_boundary(&img, 100, 100);
}

#[test]
fn vector_image_is_sharp_at_a_device_pixel_ratio() {
    // 100×100 CSS px at `dpr: 2` is 200×200 device px: the SVG must be rendered
    // at the device size, not at the CSS size and then doubled.
    let img = render(
        &list(100.0, Rect::from_xywh(0.0, 0.0, 100.0, 100.0)),
        &RasterOptions {
            scale: 2.0,
            ..RasterOptions::default()
        },
    );
    assert_eq!((img.width, img.height), (200, 200));
    assert_sharp_boundary(&img, 100, 100);
}

#[test]
fn vector_image_under_a_css_transform_is_sharp() {
    // The device size comes from the CTM, so a `transform: scale(4)` on the
    // element rasterizes at 4× too — a 50×50 box scaled to 200×200.
    let mut list = list(200.0, Rect::from_xywh(0.0, 0.0, 50.0, 50.0));
    list.items.insert(
        0,
        DisplayItem::PushLayer {
            opacity: 1.0,
            transform: oxidepage_base::Transform2D::scale(4.0, 4.0),
        },
    );
    list.items.push(DisplayItem::PopLayer);
    let img = render(&list, &RasterOptions::default());
    assert_sharp_boundary(&img, 100, 100);
}

#[test]
fn malformed_vector_image_paints_nothing() {
    // A corrupt SVG cannot be rasterized at draw time (there is no earlier decode
    // to have caught it). It must be skipped, leaving the base untouched.
    let mut list = list(100.0, Rect::from_xywh(0.0, 0.0, 100.0, 100.0));
    list.resources.images = vec![Arc::new(DecodedImage {
        id: ImageId(1),
        width: 2,
        height: 2,
        data: ImageData::Vector {
            svg: Arc::new(b"not an svg at all".to_vec()),
        },
    })];
    let img = render(&list, &RasterOptions::default());
    assert_eq!(
        img.pixel(50, 50),
        [255, 255, 255, 255],
        "unchanged white base"
    );
}
