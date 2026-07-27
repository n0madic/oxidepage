//! ResizeObserver / IntersectionObserver delivery through the page event loop
//! (ADR-0011): initial delivery under `settle`, re-delivery on layout change,
//! convergence when a callback mutates layout, and registry reset on navigation.

use oxidepage_page::{PageOptions, load_html_page};

const DOC: &str = "<!DOCTYPE html><html><head><style>\
    #box { width: 100px; height: 50px; }\
    </style></head><body><div id=\"box\"></div></body></html>";

fn page() -> oxidepage_page::Page {
    load_html_page(DOC, PageOptions::default()).unwrap()
}

/// A `ResizeObserver` reports the target's initial size once the loop settles.
#[test]
fn resize_observer_delivers_initial_size() {
    let page = page();
    page.eval(
        "window.roLog = [];\
         window.ro = new ResizeObserver((entries, obs) => {\
             for (const e of entries) {\
                 window.roLog.push(Math.round(e.contentRect.width) + 'x'\
                     + Math.round(e.contentRect.height));\
                 window.sawTarget = (e.target === document.getElementById('box'));\
                 window.sawObserver = (obs === window.ro);\
                 window.inlineSize = e.borderBoxSize[0].inlineSize;\
             }\
         });\
         window.ro.observe(document.getElementById('box'));",
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(
        page.eval_to_string("window.roLog.join('|')").unwrap(),
        "100x50"
    );
    assert_eq!(page.eval_to_string("window.sawTarget").unwrap(), "true");
    assert_eq!(page.eval_to_string("window.sawObserver").unwrap(), "true");
    // borderBoxSize is a frozen array of {inlineSize, blockSize}.
    assert_eq!(page.eval_to_string("window.inlineSize").unwrap(), "100");
    assert_eq!(
        page.eval_to_string("Object.isFrozen(document.body) === false")
            .unwrap(),
        "true"
    );
}

/// A style change that resizes the element triggers a second delivery.
#[test]
fn resize_observer_redelivers_on_layout_change() {
    let page = page();
    page.eval(
        "window.roLog = [];\
         window.ro = new ResizeObserver((entries) => {\
             for (const e of entries) window.roLog.push(Math.round(e.contentRect.width));\
         });\
         window.ro.observe(document.getElementById('box'));",
    )
    .unwrap();
    page.run_until_stalled();
    page.eval("document.getElementById('box').style.width = '200px';")
        .unwrap();
    page.run_until_stalled();
    assert_eq!(
        page.eval_to_string("window.roLog.join('|')").unwrap(),
        "100|200"
    );
}

/// A callback that mutates layout converges: the loop delivers the follow-up
/// change and then stops, rather than spinning forever.
#[test]
fn resize_observer_callback_mutation_converges() {
    let page = page();
    page.eval(
        "window.n = 0;\
         window.ro = new ResizeObserver((entries) => {\
             window.n++;\
             if (window.n === 1) document.getElementById('box').style.width = '150px';\
         });\
         window.ro.observe(document.getElementById('box'));",
    )
    .unwrap();
    page.run_until_stalled();
    // Initial delivery (n=1) grew the box; the loop delivered the new size
    // (n=2) and then converged (no further change).
    assert_eq!(page.eval_to_string("window.n").unwrap(), "2");
    assert_eq!(
        page.eval_to_string(
            "Math.round(document.getElementById('box').getBoundingClientRect().width)"
        )
        .unwrap(),
        "150"
    );
}

/// Navigation clears the observer registry: the previous document's observer
/// never fires against the new tree.
#[test]
fn navigation_resets_the_observer_registry() {
    let page = page();
    page.eval(
        "window.n = 0;\
         window.ro = new ResizeObserver(() => window.n++);\
         window.ro.observe(document.getElementById('box'));",
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(page.eval_to_string("window.n").unwrap(), "1");

    page.load_html("<!DOCTYPE html><html><body>second</body></html>")
        .unwrap();
    page.run_until_stalled();
    // The registry was cleared on navigation; the stale observer did not fire.
    assert_eq!(page.eval_to_string("window.n").unwrap(), "1");
}

/// `unobserve`/`disconnect` stop further deliveries.
#[test]
fn resize_observer_unobserve_and_disconnect() {
    let page = page();
    page.eval(
        "window.n = 0;\
         window.ro = new ResizeObserver(() => window.n++);\
         window.ro.observe(document.getElementById('box'));",
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(page.eval_to_string("window.n").unwrap(), "1");
    page.eval("window.ro.disconnect(); document.getElementById('box').style.width = '300px';")
        .unwrap();
    page.run_until_stalled();
    // Disconnected: the resize is not reported.
    assert_eq!(page.eval_to_string("window.n").unwrap(), "1");
}

// === IntersectionObserver ===

const TALL: &str = "<!DOCTYPE html><html><head><style>\
    body { margin: 0; }\
    #spacer { height: 2000px; }\
    #target { width: 100px; height: 100px; }\
    </style></head><body><div id=\"spacer\"></div><div id=\"target\"></div></body></html>";

fn tall_page() -> oxidepage_page::Page {
    load_html_page(TALL, PageOptions::default()).unwrap()
}

/// A target below the fold reports not-intersecting initially, then flips to
/// intersecting once scrolled into view.
#[test]
fn intersection_observer_flips_on_scroll() {
    let page = tall_page();
    page.eval(
        "window.ioLog = [];\
         window.io = new IntersectionObserver((entries) => {\
             for (const e of entries) window.ioLog.push(e.isIntersecting ? 'in' : 'out');\
         });\
         window.io.observe(document.getElementById('target'));",
    )
    .unwrap();
    page.run_until_stalled();
    // Initial: the target sits at y=2000, well below the 600px viewport.
    assert_eq!(
        page.eval_to_string("window.ioLog.join('|')").unwrap(),
        "out"
    );

    page.eval("window.scrollTo(0, 2000);").unwrap();
    page.run_until_stalled();
    // Scrolled into view (clamped): the target now intersects the viewport.
    assert_eq!(
        page.eval_to_string("window.ioLog.join('|')").unwrap(),
        "out|in"
    );
    assert_eq!(
        page.eval_to_string("window.io.takeRecords().length")
            .unwrap(),
        "0",
        "delivery already drained the records"
    );
}

/// A visible target reports a full intersection ratio and the entry rects.
#[test]
fn intersection_observer_reports_ratio_and_rects() {
    let page = page(); // #box is 100x50 at the top-left, fully visible.
    page.eval(
        "window.ratio = -1; window.hasRects = false;\
         window.io = new IntersectionObserver((entries) => {\
             const e = entries[0];\
             window.ratio = e.intersectionRatio;\
             window.hasRects = (e.boundingClientRect.width === 100 \
                 && e.rootBounds.width > 0 && e.intersectionRect.width === 100);\
         });\
         window.io.observe(document.getElementById('box'));",
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(
        page.eval_to_string("Math.round(window.ratio)").unwrap(),
        "1"
    );
    assert_eq!(page.eval_to_string("window.hasRects").unwrap(), "true");
}

/// `rootMargin` grows the root rectangle, bringing an otherwise out-of-view
/// target into intersection.
#[test]
fn intersection_observer_root_margin_expands_root() {
    // #target sits just below the 600px viewport (spacer 650px).
    let doc = "<!DOCTYPE html><html><head><style>\
        body { margin: 0; }\
        #spacer { height: 650px; }\
        #target { width: 100px; height: 50px; }\
        </style></head><body><div id=\"spacer\"></div><div id=\"target\"></div></body></html>";
    let page = load_html_page(doc, PageOptions::default()).unwrap();
    page.eval(
        "window.plain = null; window.margin = null;\
         new IntersectionObserver((es) => { window.plain = es[0].isIntersecting; })\
             .observe(document.getElementById('target'));\
         new IntersectionObserver((es) => { window.margin = es[0].isIntersecting; },\
             { rootMargin: '100px' })\
             .observe(document.getElementById('target'));",
    )
    .unwrap();
    page.run_until_stalled();
    // Without margin the target is below the viewport; a 100px bottom margin
    // pulls it into the (expanded) root.
    assert_eq!(page.eval_to_string("window.plain").unwrap(), "false");
    assert_eq!(page.eval_to_string("window.margin").unwrap(), "true");
}

/// Under `whole_document_visible` the implicit root spans the document, so a
/// target far below the fold is *in* view: real pages gate real content on
/// being observed (a sponsor grid that renders only once it scrolls in), and
/// the document never scrolls, so a full-page capture would otherwise be
/// missing it. An explicit element root is unaffected — only the implicit one
/// grows.
#[test]
fn whole_document_visible_makes_below_the_fold_targets_intersect() {
    // `#box` is a 100px band at the top, nowhere near `#target` at y=2000.
    const DOC: &str = "<!DOCTYPE html><html><head><style>\
        body { margin: 0; }\
        #box { height: 100px; }\
        #spacer { height: 1900px; }\
        #target { width: 100px; height: 100px; }\
        </style></head><body><div id=\"box\"></div><div id=\"spacer\"></div>\
        <div id=\"target\"></div></body></html>";
    let observe = "window.viewportRoot = null; window.elementRoot = null;\
         new IntersectionObserver((es) => { window.viewportRoot = es[0].isIntersecting; })\
             .observe(document.getElementById('target'));\
         new IntersectionObserver((es) => { window.elementRoot = es[0].isIntersecting; },\
             { root: document.getElementById('box') })\
             .observe(document.getElementById('target'));";

    let page = load_html_page(
        DOC,
        PageOptions {
            whole_document_visible: true,
            ..PageOptions::default()
        },
    )
    .unwrap();
    page.eval(observe).unwrap();
    page.run_until_stalled();
    assert_eq!(page.eval_to_string("window.viewportRoot").unwrap(), "true");
    assert_eq!(
        page.eval_to_string("window.elementRoot").unwrap(),
        "false",
        "only the implicit root grows; an explicit one stays its own padding box"
    );

    // The default is unchanged: the viewport is the root, as the spec says.
    let default_page = load_html_page(DOC, PageOptions::default()).unwrap();
    default_page.eval(observe).unwrap();
    default_page.run_until_stalled();
    assert_eq!(
        default_page.eval_to_string("window.viewportRoot").unwrap(),
        "false"
    );
}

// === Code-review regressions (2026-07-11) ===

/// `ResizeObserverEntry.contentRect` is element-local: x/y are the padding
/// offsets, not viewport-absolute coordinates.
#[test]
fn resize_observer_content_rect_is_element_local() {
    let doc = "<!DOCTYPE html><html><head><style>\
        body { margin: 0; }\
        #spacer { height: 500px; }\
        #pad { padding: 10px 20px; width: 100px; height: 50px; }\
        </style></head><body><div id=\"spacer\"></div><div id=\"pad\"></div></body></html>";
    let page = load_html_page(doc, PageOptions::default()).unwrap();
    page.eval(
        "window.rect = null;\
         new ResizeObserver((es) => {\
             const r = es[0].contentRect;\
             window.rect = [r.x, r.y, r.width, r.height].join(',');\
         }).observe(document.getElementById('pad'));",
    )
    .unwrap();
    page.run_until_stalled();
    // x=paddingLeft(20), y=paddingTop(10) — NOT the ~500px absolute page offset.
    assert_eq!(page.eval_to_string("window.rect").unwrap(), "20,10,100,50");
}

/// Removing an observed element (freeing its node) must not panic the delivery
/// loop; the observer stays usable.
#[test]
fn observers_survive_target_removal() {
    let page = tall_page();
    page.eval(
        "window.io = new IntersectionObserver(() => {});\
         window.io.observe(document.getElementById('target'));\
         window.ro = new ResizeObserver(() => {});\
         window.ro.observe(document.getElementById('target'));",
    )
    .unwrap();
    page.run_until_stalled();
    // Remove the target and drop transient references, then force delivery.
    page.eval("document.getElementById('target').remove();")
        .unwrap();
    page.run_until_stalled();
    page.eval("window.scrollTo(0, 100);").unwrap();
    page.run_until_stalled();
    // No panic; the observers are still live objects.
    assert_eq!(
        page.eval_to_string("typeof window.io.takeRecords").unwrap(),
        "function"
    );
}
