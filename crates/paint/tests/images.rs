//! WP-L: image display items — `<img>` over the content box, broken-image
//! placeholders, and background `url()` layers mapped to tile modes.

use std::sync::Arc;

use oxidepage_dom::{ParseOptions, parse_document};
use oxidepage_layout::LayoutEngine;
use oxidepage_paint::{
    Brush, Color, DisplayItem, DisplayList, PaintOptions, TileMode, build_display_list,
};
use oxidepage_style::{StyleEngine, Viewport};

const URL: &str = "http://example.com/a.png";

/// Builds the display list for `html`, optionally inserting a `w`×`h` image
/// for [`URL`] first.
fn display_list(html: &str, image: Option<(u32, u32)>) -> DisplayList {
    let mut dom = parse_document(html, ParseOptions::default()).tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut engine = LayoutEngine::new(Viewport::default());
    if let Some((w, h)) = image {
        engine.insert_raster_image(
            URL.to_string(),
            w,
            h,
            Arc::new(vec![0u8; (w * h * 4) as usize]),
        );
    }
    engine.reflow(&mut dom, &mut style);
    build_display_list(&dom, &engine, &PaintOptions::default())
}

fn image_item(list: &DisplayList) -> Option<(oxidepage_base::Rect, TileMode)> {
    list.items.iter().find_map(|i| match i {
        DisplayItem::Image { dst, tile, .. } => Some((*dst, *tile)),
        _ => None,
    })
}

#[test]
fn img_emits_image_over_content_box() {
    let list = display_list(
        "<body style='margin:0'><img style='display:block;width:100px;height:50px' src='http://example.com/a.png'></body>",
        Some((100, 50)),
    );
    let (dst, tile) = image_item(&list).expect("an image item");
    assert_eq!(dst, oxidepage_base::Rect::from_xywh(0.0, 0.0, 100.0, 50.0));
    assert_eq!(tile, TileMode::Stretch);
    assert_eq!(list.resources.images.len(), 1);
    assert_eq!(
        (
            list.resources.images[0].width,
            list.resources.images[0].height
        ),
        (100, 50)
    );
}

#[test]
fn broken_img_emits_gray_placeholder() {
    // A sized <img> with no decoded data → gray placeholder fill, no image.
    let list = display_list(
        "<body style='margin:0'><img style='display:block;width:40px;height:40px' src='http://example.com/missing.png'></body>",
        None,
    );
    assert!(image_item(&list).is_none(), "no image item without data");
    let gray = list.items.iter().any(|i| {
        matches!(
            i,
            DisplayItem::Fill { brush: Brush::Solid(c), rect, .. }
                if *c == Color::rgb(192, 192, 192) && rect.size.width == 40.0
        )
    });
    assert!(gray, "gray placeholder present: {:?}", list.items);
}

#[test]
fn background_url_no_repeat_emits_stretch_image() {
    let list = display_list(
        "<body style='margin:0'><div style='width:100px;height:100px;background:url(http://example.com/a.png) no-repeat'></div></body>",
        Some((50, 50)),
    );
    let (_, tile) = image_item(&list).expect("a background image item");
    assert_eq!(tile, TileMode::Stretch);
    assert_eq!(list.resources.images.len(), 1);
}

#[test]
fn oversized_no_repeat_background_is_clipped_to_its_box() {
    let list = display_list(
        "<body style='margin:0'><div style='width:100px;height:50px;\
         background:url(http://example.com/a.png) center/cover no-repeat'></div></body>",
        Some((50, 100)),
    );
    let (dst, tile) = image_item(&list).expect("a background image item");
    assert_eq!(tile, TileMode::Stretch);
    assert!(
        dst.origin.y < 0.0 && dst.size.height > 50.0,
        "cover tile: {dst:?}"
    );
    assert!(
        list.items
            .iter()
            .any(|item| matches!(item, DisplayItem::PushClip { rect, .. } if *rect == oxidepage_base::Rect::from_xywh(0.0, 0.0, 100.0, 50.0))),
        "oversized no-repeat background must be clipped: {:?}",
        list.items
    );
}

#[test]
fn background_url_repeat_maps_to_tile_mode() {
    let list = display_list(
        "<body style='margin:0'><div style='width:100px;height:100px;background:url(http://example.com/a.png) repeat'></div></body>",
        Some((50, 50)),
    );
    let (_, tile) = image_item(&list).expect("a background image item");
    assert_eq!(tile, TileMode::Repeat);
    // A repeated background is wrapped in a clip pair.
    assert!(
        list.items
            .iter()
            .any(|i| matches!(i, DisplayItem::PushClip { .. }))
    );
}

#[test]
fn unloaded_background_url_paints_nothing() {
    let list = display_list(
        "<body style='margin:0'><div style='width:100px;height:100px;background:url(http://example.com/a.png)'></div></body>",
        None,
    );
    assert!(image_item(&list).is_none(), "no image until it loads");
}
