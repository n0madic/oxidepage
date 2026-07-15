//! WP-H: requestAnimationFrame + "update the rendering" + screenshot.

use std::time::Duration;

use oxidepage_page::{PageOptions, load_html_page};

fn page(html: &str) -> oxidepage_page::Page {
    load_html_page(html, PageOptions::default()).unwrap()
}

#[test]
fn raf_callback_runs_with_timestamp() {
    let page = page(
        "<!DOCTYPE html><body><script>\
           window.__ran = false; window.__ts = -1;\
           requestAnimationFrame((t) => { window.__ran = true; window.__ts = t; });\
         </script></body>",
    );
    page.settle(Duration::from_millis(500));
    assert_eq!(page.eval_to_string("window.__ran").unwrap(), "true");
    // The timestamp is a finite number ≥ 0.
    assert_eq!(
        page.eval_to_string("typeof window.__ts === 'number' && window.__ts >= 0")
            .unwrap(),
        "true"
    );
}

#[test]
fn cancel_animation_frame_prevents_callback() {
    let page = page(
        "<!DOCTYPE html><body><script>\
           window.__ran = false;\
           const id = requestAnimationFrame(() => { window.__ran = true; });\
           cancelAnimationFrame(id);\
         </script></body>",
    );
    page.settle(Duration::from_millis(500));
    assert_eq!(page.eval_to_string("window.__ran").unwrap(), "false");
}

#[test]
fn re_registered_raf_runs_next_frame() {
    // A callback that re-registers itself runs across frames, counting up.
    let page = page(
        "<!DOCTYPE html><body><script>\
           window.__n = 0;\
           function tick() { window.__n++; if (window.__n < 3) requestAnimationFrame(tick); }\
           requestAnimationFrame(tick);\
         </script></body>",
    );
    page.settle(Duration::from_millis(500));
    assert_eq!(page.eval_to_string("window.__n").unwrap(), "3");
}

#[test]
fn endless_raf_is_bounded_by_settle_budget() {
    // An infinite rAF loop must not hang settle; the budget bounds it.
    let page = page(
        "<!DOCTYPE html><body><script>\
           window.__n = 0;\
           function loop() { window.__n++; requestAnimationFrame(loop); }\
           requestAnimationFrame(loop);\
         </script></body>",
    );
    let start = std::time::Instant::now();
    page.settle(Duration::from_millis(200));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "settle returned promptly"
    );
    // It ran at least a couple of frames.
    let n: i64 = page.eval_to_string("window.__n").unwrap().parse().unwrap();
    assert!(n >= 1, "ran {n} frames");
}

#[test]
fn raf_mutation_is_visible_in_screenshot() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
           <div id=d style='width:800px;height:600px;background:#ffffff'></div>\
           <script>\
             requestAnimationFrame(() => {\
               document.getElementById('d').style.background = '#ff0000';\
             });\
           </script></body>",
    );
    page.settle(Duration::from_millis(500));
    let png = page.screenshot(1.0);
    assert!(!png.is_empty());
    assert_eq!(&png[0..4], &[0x89, b'P', b'N', b'G']);

    // Re-render pixels and check the div turned red.
    let img = page.render_pixels(&oxidepage_page::RasterOptions::default());
    let p = img.pixel(400, 300);
    assert!(p[0] > 200 && p[1] < 60 && p[2] < 60, "center {p:?}");
}

#[test]
fn screenshot_dpr_scales_dimensions() {
    let page = page("<!DOCTYPE html><body></body>");
    let img = page.render_pixels(&oxidepage_page::RasterOptions {
        scale: 2.0,
        ..oxidepage_page::RasterOptions::default()
    });
    assert_eq!(img.width, 1600);
    assert_eq!(img.height, 1200);
}
