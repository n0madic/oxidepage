//! WP-D: display-list caching and invalidation on the page.

use std::sync::Arc;

use oxidepage_page::{PageOptions, load_html_page};

#[test]
fn cache_returns_same_arc_while_clean() {
    let page = load_html_page(
        "<!DOCTYPE html><body><div style='background:#ff0000;width:10px;height:10px'></div></body>",
        PageOptions::default(),
    )
    .unwrap();
    let a = page.display_list();
    let b = page.display_list();
    assert!(
        Arc::ptr_eq(&a, &b),
        "clean page reuses the cached display list"
    );
}

#[test]
fn style_mutation_invalidates_cache() {
    let page = load_html_page(
        "<!DOCTYPE html><body><div id=d style='width:10px;height:10px'></div></body>",
        PageOptions::default(),
    )
    .unwrap();
    let before = page.display_list();
    page.eval_to_string("document.getElementById('d').style.background='#00ff00'")
        .unwrap();
    let after = page.display_list();
    assert!(
        !Arc::ptr_eq(&before, &after),
        "a style mutation must rebuild the display list"
    );
}

#[test]
fn document_scroll_reuses_cached_display_list() {
    // Content taller than the viewport, so a scroll actually moves. The display
    // list is built unscrolled and the rasterizer applies the document scroll,
    // so scrolling the document must NOT rebuild (or even re-key) the list — the
    // same cached `Arc` is served at every scroll offset.
    let page = load_html_page(
        "<!DOCTYPE html><body><div style='height:3000px;background:#ff0000'></div></body>",
        PageOptions::default(),
    )
    .unwrap();
    let before = page.display_list();
    page.eval_to_string("window.scrollTo(0, 200)").unwrap();
    assert_eq!(
        page.eval_to_string("window.scrollY").unwrap(),
        "200",
        "the page actually scrolled"
    );
    let after = page.display_list();
    assert!(
        Arc::ptr_eq(&before, &after),
        "document scroll must reuse the cached display list, not rebuild it"
    );
}

#[test]
fn element_scroll_invalidates_cache() {
    // An element's overflow scroll IS baked into item origins, so it still must
    // dirty paint and rebuild the display list (unlike document scroll above).
    let page = load_html_page(
        "<!DOCTYPE html><body>\
           <div id=s style='width:100px;height:100px;overflow:scroll'>\
             <div style='width:100px;height:1000px;background:#ff0000'></div>\
           </div>\
         </body>",
        PageOptions::default(),
    )
    .unwrap();
    let before = page.display_list();
    page.eval_to_string("document.getElementById('s').scrollTop = 200")
        .unwrap();
    let after = page.display_list();
    assert!(
        !Arc::ptr_eq(&before, &after),
        "an element overflow scroll must rebuild the display list"
    );
}

#[test]
fn json_dump_round_trips() {
    let page = load_html_page(
        "<!DOCTYPE html><body><div style='background:#ff0000;width:10px;height:10px'></div></body>",
        PageOptions::default(),
    )
    .unwrap();
    let json = page.display_list_json();
    assert!(json.contains("\"viewport\""));
    assert!(json.contains("\"items\""));
    // The red div background appears in the dump.
    assert!(json.contains("#ff0000ff"), "{json}");
}
