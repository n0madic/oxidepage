//! WP-F: pixel-level raster tests — solid fills, rounded-corner transparency,
//! clipping, opacity compositing, gradient endpoints, DPR scaling, and PNG.

use oxidepage_base::{Rect, Size, Transform2D};
use oxidepage_paint::{
    BorderRadii, Brush, Color, DisplayItem, DisplayList, GradientStop, LinearGradient,
    ResourceTable,
};
use oxidepage_raster_skia::{RasterOptions, encode_png, render};

fn list(items: Vec<DisplayItem>) -> DisplayList {
    DisplayList {
        viewport: Size::new(100.0, 100.0),
        content_size: Size::new(100.0, 100.0),
        items,
        resources: ResourceTable::default(),
    }
}

fn solid(rect: Rect, radii: BorderRadii, color: Color) -> DisplayItem {
    DisplayItem::Fill {
        rect,
        radii,
        brush: Brush::Solid(color),
    }
}

const RED: Color = Color::rgb(255, 0, 0);

#[test]
fn solid_fill_paints_the_rect() {
    let img = render(
        &list(vec![solid(
            Rect::from_xywh(10.0, 10.0, 30.0, 30.0),
            BorderRadii::ZERO,
            RED,
        )]),
        &RasterOptions::default(),
    );
    // Inside the rect: red. Outside: white base.
    assert_eq!(img.pixel(25, 25), [255, 0, 0, 255]);
    assert_eq!(img.pixel(5, 5), [255, 255, 255, 255]);
    assert_eq!(img.pixel(50, 50), [255, 255, 255, 255]);
}

#[test]
fn rounded_corner_is_not_painted() {
    // A big rounded rect: its very corner pixel stays the white base.
    let img = render(
        &list(vec![solid(
            Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
            BorderRadii::uniform(40.0),
            RED,
        )]),
        &RasterOptions::default(),
    );
    // Center is red; the extreme corner is outside the rounded shape.
    assert_eq!(img.pixel(50, 50), [255, 0, 0, 255]);
    assert_eq!(img.pixel(0, 0), [255, 255, 255, 255]);
}

#[test]
fn clip_limits_painting() {
    let img = render(
        &list(vec![
            DisplayItem::PushClip {
                rect: Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
                radii: BorderRadii::ZERO,
            },
            solid(
                Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
                BorderRadii::ZERO,
                RED,
            ),
            DisplayItem::PopClip,
        ]),
        &RasterOptions::default(),
    );
    assert_eq!(img.pixel(10, 10), [255, 0, 0, 255], "inside clip is red");
    assert_eq!(
        img.pixel(50, 50),
        [255, 255, 255, 255],
        "outside clip stays white"
    );
}

#[test]
fn opacity_layer_blends_half() {
    let img = render(
        &list(vec![
            DisplayItem::PushLayer {
                opacity: 0.5,
                transform: Transform2D::IDENTITY,
            },
            solid(
                Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
                BorderRadii::ZERO,
                RED,
            ),
            DisplayItem::PopLayer,
        ]),
        &RasterOptions::default(),
    );
    // 50% red over white → (255, 127, 127).
    let p = img.pixel(50, 50);
    assert!((p[0] as i32 - 255).abs() <= 1, "r {}", p[0]);
    assert!((p[1] as i32 - 128).abs() <= 3, "g {}", p[1]);
    assert!((p[2] as i32 - 128).abs() <= 3, "b {}", p[2]);
}

#[test]
fn linear_gradient_endpoints_have_end_colors() {
    let img = render(
        &list(vec![DisplayItem::Fill {
            rect: Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
            radii: BorderRadii::ZERO,
            brush: Brush::LinearGradient(LinearGradient {
                start: oxidepage_base::Point::new(0.0, 50.0),
                end: oxidepage_base::Point::new(100.0, 50.0),
                stops: vec![
                    GradientStop {
                        offset: 0.0,
                        color: Color::rgb(255, 0, 0),
                    },
                    GradientStop {
                        offset: 1.0,
                        color: Color::rgb(0, 0, 255),
                    },
                ],
                extend: oxidepage_paint::ExtendMode::Pad,
            }),
        }]),
        &RasterOptions::default(),
    );
    // Left edge red-ish, right edge blue-ish.
    let left = img.pixel(1, 50);
    let right = img.pixel(98, 50);
    assert!(left[0] > 200 && left[2] < 60, "left {left:?}");
    assert!(right[2] > 200 && right[0] < 60, "right {right:?}");
}

#[test]
fn dpr_scales_output() {
    let img = render(
        &list(vec![solid(
            Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
            BorderRadii::ZERO,
            RED,
        )]),
        &RasterOptions {
            scale: 2.0,
            ..RasterOptions::default()
        },
    );
    assert_eq!(img.width, 200);
    assert_eq!(img.height, 200);
    // The fill covers the whole (scaled) surface.
    assert_eq!(img.pixel(150, 150), [255, 0, 0, 255]);
}

#[test]
fn single_stop_gradient_fills_solid() {
    // A degenerate one-stop gradient must paint a solid fill of that color
    // (matching the PDF backend), not nothing (finding L2).
    let img = render(
        &list(vec![DisplayItem::Fill {
            rect: Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
            radii: BorderRadii::ZERO,
            brush: Brush::LinearGradient(LinearGradient {
                start: oxidepage_base::Point::new(0.0, 0.0),
                end: oxidepage_base::Point::new(100.0, 0.0),
                stops: vec![GradientStop {
                    offset: 0.0,
                    color: Color::rgb(0, 128, 255),
                }],
                extend: oxidepage_paint::ExtendMode::Pad,
            }),
        }]),
        &RasterOptions::default(),
    );
    assert_eq!(
        img.pixel(50, 50),
        [0, 128, 255, 255],
        "one-stop gradient fills solid with the stop color"
    );
}

#[test]
fn absurd_viewport_does_not_panic() {
    // An enormous viewport × dpr must not `expect()`-panic on allocation; the
    // device size is clamped and a bounded image is returned (finding L3).
    let mut list = list(vec![solid(
        Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
        BorderRadii::ZERO,
        RED,
    )]);
    list.viewport = Size::new(1_000_000.0, 1_000_000.0);
    let img = render(
        &list,
        &RasterOptions {
            scale: 1000.0,
            ..RasterOptions::default()
        },
    );
    // Completed without panicking, with clamped (bounded) dimensions.
    assert!(img.width <= 16_384 && img.height <= 16_384, "clamped dims");
    assert!(
        u64::from(img.width) * u64::from(img.height) <= 64_000_000,
        "clamped area"
    );
}

#[test]
fn deeply_nested_layers_and_clips_render_bounded() {
    // Hundreds of nested opacity layers + clips: naive per-level full-canvas
    // allocations would be gigabytes. It must render (bounded) and not OOM.
    // Each layer is sized to the (sub-canvas) clip, and the innermost red is
    // still visible after compositing back up (finding H1).
    let mut items = Vec::new();
    for _ in 0..400 {
        items.push(DisplayItem::PushLayer {
            opacity: 1.0,
            transform: Transform2D::IDENTITY,
        });
        items.push(DisplayItem::PushClip {
            rect: Rect::from_xywh(20.0, 20.0, 60.0, 60.0),
            radii: BorderRadii::ZERO,
        });
    }
    items.push(solid(
        Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
        BorderRadii::ZERO,
        RED,
    ));
    for _ in 0..400 {
        items.push(DisplayItem::PopClip);
        items.push(DisplayItem::PopLayer);
    }
    let img = render(&list(items), &RasterOptions::default());
    // Inside the nested clip the red shows through; outside stays white.
    assert_eq!(img.pixel(50, 50), [255, 0, 0, 255], "clipped red visible");
    assert_eq!(
        img.pixel(5, 5),
        [255, 255, 255, 255],
        "outside clip is white"
    );
}

#[test]
fn png_round_trips() {
    let img = render(
        &list(vec![solid(
            Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
            BorderRadii::ZERO,
            RED,
        )]),
        &RasterOptions::default(),
    );
    let png = encode_png(&img).expect("encode");
    // PNG magic bytes.
    assert_eq!(
        &png[0..8],
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
    );
    let decoded = tiny_skia_decode(&png);
    assert_eq!(decoded, (100, 100));
}

/// Decodes a PNG's dimensions via the png crate (transitive dep) for the
/// round-trip check.
fn tiny_skia_decode(bytes: &[u8]) -> (u32, u32) {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let reader = decoder.read_info().expect("png header");
    let info = reader.info();
    (info.width, info.height)
}
