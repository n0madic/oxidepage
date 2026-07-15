//! WP-L: image rasterization — pixel blitting (stretch) and tiling (repeat).

use std::sync::Arc;

use oxidepage_base::{Rect, Size};
use oxidepage_paint::{
    BorderRadii, DecodedImage, DisplayItem, DisplayList, ImageData, ImageId, ResourceTable,
    TileMode,
};
use oxidepage_raster_skia::{RasterOptions, render};

/// A 2×2 image: top-left red, top-right green, bottom-left blue, bottom-right
/// white (straight-alpha RGBA).
fn checker_image(id: u64) -> Arc<DecodedImage> {
    #[rustfmt::skip]
    let rgba = vec![
        255, 0, 0, 255,   0, 255, 0, 255,
        0, 0, 255, 255,   255, 255, 255, 255,
    ];
    Arc::new(DecodedImage {
        id: ImageId(id),
        width: 2,
        height: 2,
        data: ImageData::Raster {
            rgba: Arc::new(rgba),
        },
    })
}

/// Whether a pixel is approximately `expected` (bilinear scaling of a small
/// image introduces minor interpolation).
fn near(pixel: [u8; 4], expected: [u8; 4]) -> bool {
    (0..4).all(|i| (i32::from(pixel[i]) - i32::from(expected[i])).abs() <= 16)
}

fn list(items: Vec<DisplayItem>, images: Vec<Arc<DecodedImage>>) -> DisplayList {
    DisplayList {
        viewport: Size::new(100.0, 100.0),
        content_size: Size::new(100.0, 100.0),
        items,
        resources: ResourceTable {
            images,
            ..ResourceTable::default()
        },
    }
}

#[test]
fn stretched_image_blits_scaled_pixels() {
    // Scale the 2×2 checker up to 100×100; each quadrant is 50×50.
    let image = checker_image(1);
    let img = render(
        &list(
            vec![DisplayItem::Image {
                dst: Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
                image: ImageId(1),
                tile: TileMode::Stretch,
                radii: BorderRadii::ZERO,
            }],
            vec![image],
        ),
        &RasterOptions::default(),
    );
    // Sample the middle of each quadrant.
    assert!(
        near(img.pixel(25, 25), [255, 0, 0, 255]),
        "top-left red {:?}",
        img.pixel(25, 25)
    );
    assert!(near(img.pixel(75, 25), [0, 255, 0, 255]), "top-right green");
    assert!(
        near(img.pixel(25, 75), [0, 0, 255, 255]),
        "bottom-left blue"
    );
    assert!(
        near(img.pixel(75, 75), [255, 255, 255, 255]),
        "bottom-right white"
    );
}

#[test]
fn repeated_image_tiles_across_clip() {
    // A 2×2 tile repeated within a 100×100 clip: the pattern wraps, so the
    // red top-left pixel of the tile recurs every 2 device px.
    let image = checker_image(2);
    let img = render(
        &list(
            vec![
                DisplayItem::PushClip {
                    rect: Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
                    radii: BorderRadii::ZERO,
                },
                DisplayItem::Image {
                    dst: Rect::from_xywh(0.0, 0.0, 2.0, 2.0),
                    image: ImageId(2),
                    tile: TileMode::Repeat,
                    radii: BorderRadii::ZERO,
                },
                DisplayItem::PopClip,
            ],
            vec![image],
        ),
        &RasterOptions::default(),
    );
    // The tile's red pixel recurs; sampling far into the clip is still colored
    // (not the white base), proving it tiled rather than drawing once.
    let p = img.pixel(60, 60);
    assert!(
        p != [255, 255, 255, 255] || img.pixel(61, 61) != [255, 255, 255, 255],
        "tile repeats across the clip, {p:?}"
    );
    // The top-left tile pixel is red.
    assert!(near(img.pixel(0, 0), [255, 0, 0, 255]));
}

#[test]
fn missing_resource_is_skipped() {
    // An image item whose id is absent from resources paints nothing (no panic).
    let img = render(
        &list(
            vec![DisplayItem::Image {
                dst: Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
                image: ImageId(99),
                tile: TileMode::Stretch,
                radii: BorderRadii::ZERO,
            }],
            vec![],
        ),
        &RasterOptions::default(),
    );
    assert_eq!(
        img.pixel(50, 50),
        [255, 255, 255, 255],
        "unchanged white base"
    );
}
