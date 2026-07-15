//! WP-J: image intrinsic sizing (captured into the replaced context from the
//! store) and store versioning.

use std::sync::Arc;

use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};
use oxidepage_layout::{LayoutEngine, ReplacedContent};
use oxidepage_style::{StyleEngine, Viewport};

const URL: &str = "http://example.com/a.png";

fn find_img(dom: &DomTree) -> NodeId {
    dom.inclusive_descendants(dom.document())
        .find(|&id| {
            dom.node(id)
                .as_element()
                .is_some_and(|el| &*el.name.local == "img")
        })
        .expect("an <img> in the document")
}

/// Lays out `html` after inserting a `w`×`h` image for [`URL`] (when `insert`).
fn layout_with_image(html: &str, insert: Option<(u32, u32)>) -> (DomTree, LayoutEngine) {
    let mut dom = parse_document(html, ParseOptions::default()).tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut engine = LayoutEngine::new(Viewport::default());
    if let Some((w, h)) = insert {
        engine.insert_raster_image(
            URL.to_string(),
            w,
            h,
            Arc::new(vec![0u8; (w * h * 4) as usize]),
        );
    }
    engine.reflow(&mut dom, &mut style);
    (dom, engine)
}

/// The replaced context captured for the document's `<img>`.
fn img_context(dom: &DomTree, engine: &LayoutEngine) -> oxidepage_layout::ReplacedContext {
    let node = find_img(dom);
    let box_id = engine.tree().box_for_node(node).expect("img box");
    match engine.tree().box_(box_id).replaced.as_ref() {
        Some(ReplacedContent::Image(ctx)) => ctx.clone(),
        other => panic!("expected image replaced content, got {other:?}"),
    }
}

#[test]
fn intrinsic_size_and_data_come_from_the_store() {
    let (dom, engine) = layout_with_image("<img src='http://example.com/a.png'>", Some((40, 20)));
    let ctx = img_context(&dom, &engine);
    assert_eq!(ctx.inherent_size.width, 40.0);
    assert_eq!(ctx.inherent_size.height, 20.0);
    let data = ctx.data.expect("decoded image attached");
    assert_eq!((data.width, data.height), (40, 20));
}

#[test]
fn attribute_sizes_are_captured() {
    let (dom, engine) = layout_with_image(
        "<img src='http://example.com/a.png' width=100 height=50>",
        Some((40, 20)),
    );
    let ctx = img_context(&dom, &engine);
    assert_eq!(ctx.attr_size.width, Some(100.0));
    assert_eq!(ctx.attr_size.height, Some(50.0));
    // The intrinsic size (from the store) is still available for aspect ratio.
    assert_eq!(ctx.inherent_size.width, 40.0);
}

#[test]
fn missing_image_has_zero_intrinsic_and_no_data() {
    let (dom, engine) = layout_with_image("<img src='http://example.com/missing.png'>", None);
    let ctx = img_context(&dom, &engine);
    assert_eq!(ctx.inherent_size.width, 0.0);
    assert_eq!(ctx.inherent_size.height, 0.0);
    assert!(ctx.data.is_none());
}

#[test]
fn inserting_image_bumps_store_and_paint_stamp() {
    let mut engine = LayoutEngine::new(Viewport::default());
    let before = engine.images().version();
    engine.insert_raster_image(URL.to_string(), 10, 10, Arc::new(vec![0; 400]));
    assert!(engine.images().version() > before);

    let v1 = engine.paint_stamp().images_version;
    engine.insert_raster_image(URL.to_string(), 20, 20, Arc::new(vec![0; 1600]));
    assert!(engine.paint_stamp().images_version > v1);
}

#[test]
fn intrinsic_size_forces_relayout_not_patch() {
    // Inserting an image after an initial layout must rebuild (new intrinsic
    // size), not take the incremental patch path.
    let mut dom = parse_document(
        "<img src='http://example.com/a.png'>",
        ParseOptions::default(),
    )
    .tree;
    let mut style = StyleEngine::new(&dom, Viewport::default());
    let mut engine = LayoutEngine::new(Viewport::default());
    engine.reflow(&mut dom, &mut style);
    let (rebuilds_before, _) = engine.reflow_counts();

    engine.insert_raster_image(URL.to_string(), 40, 20, Arc::new(vec![0; 3200]));
    engine.reflow(&mut dom, &mut style);
    let (rebuilds_after, _) = engine.reflow_counts();
    assert!(
        rebuilds_after > rebuilds_before,
        "image insert forced a rebuild"
    );

    let ctx = img_context(&dom, &engine);
    assert_eq!(ctx.inherent_size.width, 40.0);
}

#[test]
fn image_update_queue_notes_connected_img() {
    let mut dom = parse_document(
        "<img src='http://example.com/a.png'>",
        ParseOptions::default(),
    )
    .tree;
    let updates = dom.take_image_updates();
    let img = find_img(&dom);
    assert!(
        updates.contains(&img),
        "connected <img> queued: {updates:?}"
    );
}

/// A percentage width/height on a replaced element with no basis to resolve
/// against behaves as `auto` — i.e. the intrinsic size — and must not collapse
/// the element to nothing. Taffy probes intrinsic widths with the height's
/// available space set to `MinContent`, and reading that as a zero percentage
/// basis zeroed the height and then, through the aspect ratio, the width:
/// mgid.com's hero image (`width: 100%; height: 100%` in an auto-height flex
/// item) laid out 0×0.
#[test]
fn percentage_sized_image_in_auto_height_parent_uses_its_intrinsic_size() {
    let (dom, engine) = layout_with_image(
        "<div style='display:flex'>\
           <div style='flex:1 1 auto'>text side</div>\
           <div style='max-width:375px'>\
             <div style='position:relative'>\
               <img src='http://example.com/a.png' \
                    style='display:flex;width:100%;height:100%;max-width:100%'>\
             </div>\
           </div>\
         </div>",
        Some((750, 1050)),
    );

    let img = engine
        .tree()
        .box_for_node(find_img(&dom))
        .expect("a box for the <img>");
    let layout = engine.tree().box_(img).final_layout;
    // Shrunk to the flex item's 375px cap, with the intrinsic 750×1050 ratio.
    assert_eq!(layout.size.width, 375.0);
    assert_eq!(layout.size.height, 525.0);
}
