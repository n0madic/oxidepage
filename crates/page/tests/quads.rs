//! The two actionability primitives on [`Page`] (ADR-0026): `content_quads`
//! and `scroll_into_view_if_needed`. Every driver's "can I click this?" check
//! is built from the pair, so they are asserted at the embedder surface rather
//! than through JS.

use oxidepage_base::Rect;
use oxidepage_page::{Page, PageOptions, Viewport, load_html_page};

fn page_with(html: &str) -> Page {
    load_html_page(html, PageOptions::default()).expect("page")
}

fn page_with_viewport(html: &str, width: f32, height: f32) -> Page {
    load_html_page(
        html,
        PageOptions {
            viewport: Some(Viewport {
                width,
                height,
                dpr: 1.0,
            }),
            ..PageOptions::default()
        },
    )
    .expect("page")
}

fn by_id(page: &Page, id: &str) -> oxidepage_base::NodeId {
    let dom = page.dom();
    dom.element_by_id(dom.document(), id)
        .unwrap_or_else(|| panic!("no element with id={id}"))
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

#[test]
fn content_quads_of_an_untransformed_box_are_its_corners() {
    let page = page_with(
        "<!doctype html><body style='margin:0'>\
         <div id=d style='width:100px;height:40px'></div></body>",
    );
    let quads = page.content_quads(by_id(&page, "d"));
    assert_eq!(quads.len(), 1);
    let q = quads[0];
    assert_eq!((q[0].x, q[0].y), (0.0, 0.0), "top-left");
    assert_eq!((q[1].x, q[1].y), (100.0, 0.0), "top-right");
    assert_eq!((q[2].x, q[2].y), (100.0, 40.0), "bottom-right");
    assert_eq!((q[3].x, q[3].y), (0.0, 40.0), "bottom-left");
}

#[test]
fn content_quads_keep_corner_order_through_a_rotation() {
    // A 100×40 box rotated a quarter turn about its centre (50, 20): the
    // top-left corner lands at (70, -30) and the rest follow it round.
    let page = page_with(
        "<!doctype html><body style='margin:0'>\
         <div id=d style='width:100px;height:40px;transform:rotate(90deg)'></div></body>",
    );
    let q = page.content_quads(by_id(&page, "d"))[0];
    assert!(near(q[0].x, 70.0) && near(q[0].y, -30.0), "{q:?}");
    assert!(near(q[1].x, 70.0) && near(q[1].y, 70.0), "{q:?}");
    assert!(near(q[2].x, 30.0) && near(q[2].y, 70.0), "{q:?}");
    assert!(near(q[3].x, 30.0) && near(q[3].y, -30.0), "{q:?}");

    // The bounding box of that quad is what `getBoundingClientRect` reports.
    let rect = page.layout_rect(by_id(&page, "d")).unwrap();
    assert!(
        near(rect.origin.x, 30.0) && near(rect.size.width, 40.0),
        "{rect:?}"
    );
}

#[test]
fn a_wrapped_inline_reports_one_quad_per_line() {
    let page = page_with(
        "<!doctype html><body style='margin:0'>\
         <div style='font-family:Ahem;font-size:10px;line-height:10px;width:30px'>\
         <span id=s>aaa aaa</span></div></body>",
    );
    let quads = page.content_quads(by_id(&page, "s"));
    assert_eq!(quads.len(), 2, "{quads:?}");
    assert_eq!((quads[0][0].x, quads[0][0].y), (0.0, 0.0));
    assert_eq!((quads[1][0].x, quads[1][0].y), (0.0, 10.0));
}

#[test]
fn content_quads_of_a_boxless_element_are_empty() {
    let page = page_with(
        "<!doctype html><body style='margin:0'>\
         <div id=d style='display:none'></div></body>",
    );
    assert!(page.content_quads(by_id(&page, "d")).is_empty());
}

#[test]
fn scroll_into_view_if_needed_is_a_no_op_when_visible() {
    let page = page_with_viewport(
        "<!doctype html><body style='margin:0'>\
         <div id=d style='width:50px;height:50px'></div>\
         <div style='height:2000px'></div></body>",
        400.0,
        400.0,
    );
    assert!(
        !page.scroll_into_view_if_needed(by_id(&page, "d"), None),
        "an element already on screen must not scroll anything"
    );
    assert_eq!(page.eval_to_string("window.scrollY").unwrap(), "0");
}

#[test]
fn scroll_into_view_if_needed_scrolls_the_document() {
    let page = page_with_viewport(
        "<!doctype html><body style='margin:0'>\
         <div style='height:2000px'></div>\
         <div id=d style='width:50px;height:50px'></div></body>",
        400.0,
        400.0,
    );
    let d = by_id(&page, "d");
    assert!(page.scroll_into_view_if_needed(d, None));
    // `nearest`: the minimum that brings the far edge in — 2050 − 400 = 1650.
    assert_eq!(page.eval_to_string("window.scrollY").unwrap(), "1650");
    let rect = page.layout_rect(d).unwrap();
    assert_eq!(rect.origin.y, 350.0);

    // Idempotent: it is now visible, so a second call does nothing.
    assert!(!page.scroll_into_view_if_needed(d, None));
    assert_eq!(page.eval_to_string("window.scrollY").unwrap(), "1650");
}

#[test]
fn scroll_into_view_if_needed_walks_every_scroll_container() {
    // The target is off-screen in the inner scroller *and* the inner scroller is
    // off-screen in the document: revealing it has to move both.
    let page = page_with_viewport(
        "<!doctype html><body style='margin:0'>\
         <div style='height:1000px'></div>\
         <div id=scroller style='overflow:scroll;width:200px;height:100px'>\
         <div style='height:500px'></div>\
         <div id=d style='width:50px;height:50px'></div></div></body>",
        400.0,
        400.0,
    );
    let d = by_id(&page, "d");
    assert!(page.scroll_into_view_if_needed(d, None));
    assert_eq!(
        page.eval_to_string("document.getElementById('scroller').scrollTop")
            .unwrap(),
        "450",
        "the inner scroller moved"
    );
    assert_ne!(
        page.eval_to_string("window.scrollY").unwrap(),
        "0",
        "and so did the document"
    );
    // Fully visible now: inside the viewport and inside the scroller.
    let rect = page.layout_rect(d).unwrap();
    assert!(
        rect.origin.y >= 0.0 && rect.origin.y + rect.size.height <= 400.0,
        "{rect:?}"
    );
}

#[test]
fn scroll_into_view_if_needed_honours_a_sub_rect() {
    // The element itself starts on screen, so the whole-element call is a no-op;
    // asking for a sub-rect near its bottom edge, which is not, must scroll.
    let page = page_with_viewport(
        "<!doctype html><body style='margin:0'>\
         <div id=d style='width:50px;height:1000px'></div></body>",
        400.0,
        400.0,
    );
    let d = by_id(&page, "d");
    assert!(
        !page.scroll_into_view_if_needed(d, None),
        "a box taller than the viewport whose top is visible is already 'nearest'"
    );
    assert!(page.scroll_into_view_if_needed(d, Some(Rect::from_xywh(0.0, 900.0, 50.0, 50.0))));
    // The sub-rect's far edge (950) brought to the viewport bottom: 950 − 400.
    assert_eq!(page.eval_to_string("window.scrollY").unwrap(), "550");
}

#[test]
fn scroll_into_view_if_needed_queues_a_scroll_event() {
    let page = page_with_viewport(
        "<!doctype html><body style='margin:0'>\
         <div style='height:2000px'></div>\
         <div id=d style='width:50px;height:50px'></div>\
         <script>window.scrolls = 0;\
           document.addEventListener('scroll', () => window.scrolls++);</script>\
         </body>",
        400.0,
        400.0,
    );
    assert!(page.scroll_into_view_if_needed(by_id(&page, "d"), None));
    page.settle(std::time::Duration::from_millis(200));
    assert_eq!(
        page.eval_to_string("window.scrolls").unwrap(),
        "1",
        "the embedder-driven scroll fires the same event a script scroll does"
    );
}

#[test]
fn scroll_into_view_if_needed_scrolls_in_the_containers_own_space() {
    // A scroll offset is in the container's own content px, but the rects that
    // drive the decision are visual. Under a `scale(2)` ancestor the visual
    // delta is twice the scroll needed, and the container overshot by exactly
    // that factor — and a second call was no longer a no-op.
    let page = page_with_viewport(
        "<!doctype html><body style='margin:0'>\
         <div style='transform:scale(2);transform-origin:0 0'>\
         <div id=scroller style='overflow:scroll;width:100px;height:100px'>\
         <div style='height:300px'></div>\
         <div id=d style='width:50px;height:50px'></div></div></div></body>",
        800.0,
        800.0,
    );
    let d = by_id(&page, "d");
    assert!(page.scroll_into_view_if_needed(d, None));
    // In the scroller's own space the target sits at y = 300 and the scrollport
    // is 100 tall: `nearest` scrolls 300 + 50 − 100 = 250, not 500.
    assert_eq!(
        page.eval_to_string("document.getElementById('scroller').scrollTop")
            .unwrap(),
        "250"
    );
    assert!(
        !page.scroll_into_view_if_needed(d, None),
        "already revealed: a second call must not move anything"
    );
}
