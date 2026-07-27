//! Regression tests for event-loop termination and document teardown:
//! zero-delay self-reposting timers must not hang the loop (C1), pathological
//! timer delays must not panic (H-page-1), and a second load must replace the
//! previous document rather than append into it (H-page-2).

use std::time::{Duration, Instant};

use oxidepage_page::{PageOptions, load_html_page};

fn page(html: &str) -> oxidepage_page::Page {
    load_html_page(html, PageOptions::default()).unwrap()
}

#[test]
fn zero_delay_setinterval_does_not_hang_load() {
    // Before the nesting clamp this spun `run_until_stalled` forever, so
    // `load_html_page` never returned. It must complete promptly.
    let start = Instant::now();
    let page = page(
        "<!DOCTYPE html><body><script>\
           window.__n = 0;\
           setInterval(function(){ window.__n++; }, 0);\
         </script></body>",
    );
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "load returned promptly ({:?})",
        start.elapsed()
    );
    // The interval did run at least once.
    let n: i64 = page.eval_to_string("window.__n").unwrap().parse().unwrap();
    assert!(n >= 1, "interval ran {n} times");
}

#[test]
fn self_reposting_zero_delay_settimeout_is_bounded_by_settle_budget() {
    let page = page(
        "<!DOCTYPE html><body><script>\
           window.__n = 0;\
           function f(){ window.__n++; setTimeout(f, 0); }\
           f();\
         </script></body>",
    );
    let start = Instant::now();
    page.settle(Duration::from_millis(200));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "settle returned promptly ({:?})",
        start.elapsed()
    );
    let n: i64 = page.eval_to_string("window.__n").unwrap().parse().unwrap();
    assert!(n >= 1, "timer chain ran {n} times");
}

#[test]
fn non_finite_timer_delay_does_not_panic() {
    // `setTimeout(fn, Infinity)` must not panic Duration/Instant math. Per the
    // HTML `long` coercion (ToInt32) a non-finite delay becomes 0, so the
    // callback runs.
    let page = page(
        "<!DOCTYPE html><body><script>\
           window.__inf = 0; window.__nan = 0;\
           setTimeout(function(){ window.__inf = 1; }, Infinity);\
           setTimeout(function(){ window.__nan = 1; }, NaN);\
         </script></body>",
    );
    page.settle(Duration::from_millis(200));
    assert_eq!(page.eval_to_string("window.__inf").unwrap(), "1");
    assert_eq!(page.eval_to_string("window.__nan").unwrap(), "1");
    assert!(page.drain_errors().is_empty());
}

#[test]
fn huge_finite_timer_delay_does_not_panic() {
    // A finite delay far larger than `i32::MAX` ms is clamped, not overflowed.
    let page = page(
        "<!DOCTYPE html><body><script>\
           setTimeout(function(){}, 1e15);\
         </script></body>",
    );
    page.settle(Duration::from_millis(50));
    assert!(page.drain_errors().is_empty());
    assert!(!page.is_loaded() || page.is_loaded()); // no panic reaching here
}

#[test]
fn second_load_replaces_document_not_appends() {
    let page = page("<!DOCTYPE html><body><div id=a>first</div></body>");
    assert_eq!(
        page.eval_to_string("document.querySelectorAll('div').length")
            .unwrap(),
        "1"
    );

    page.load_html("<!DOCTYPE html><body><div id=b>second</div></body>")
        .unwrap();

    // The first document's element is gone; only the new one remains (a fresh
    // parse must not append into the previous tree).
    assert_eq!(
        page.eval_to_string("document.querySelectorAll('div').length")
            .unwrap(),
        "1",
        "errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(
        page.eval_to_string("document.getElementById('a') === null")
            .unwrap(),
        "true"
    );
    assert_eq!(
        page.eval_to_string("document.getElementById('b').textContent")
            .unwrap(),
        "second"
    );
    assert!(page.is_loaded());
}

/// Regression: the realm survives a navigation, so the previous document's
/// timers must be discarded with it. A surviving `setInterval` would keep
/// running doc-1's callback against doc-2 (browsers discard it).
#[test]
fn navigation_cancels_the_previous_documents_timers() {
    let page = page(
        "<!DOCTYPE html><body><script>\
           window.__ticks = 0;\
           setInterval(function(){ window.__ticks++; }, 1);\
         </script></body>",
    );

    page.load_html("<!DOCTYPE html><body>second</body>")
        .unwrap();

    // Zero the counter *after* navigating, then give the loop real time to run
    // any surviving interval.
    page.eval_to_string("window.__ticks = 0").unwrap();
    page.settle(Duration::from_millis(50));

    assert_eq!(
        page.eval_to_string("window.__ticks").unwrap(),
        "0",
        "the previous document's interval must not fire after navigation"
    );
}

/// Regression: a pending `requestAnimationFrame` from the previous document must
/// not run at the new document's rendering opportunities. The callback re-arms
/// itself, so a frame is always pending at the moment of navigation (a one-shot
/// frame would already have fired during the first load).
#[test]
fn navigation_cancels_the_previous_documents_animation_frames() {
    let page = page(
        "<!DOCTYPE html><body><script>\
           window.__frames = 0;\
           (function arm(){ requestAnimationFrame(function(){ window.__frames++; arm(); }); })();\
         </script></body>",
    );

    page.load_html("<!DOCTYPE html><body>second</body>")
        .unwrap();
    page.eval_to_string("window.__frames = 0").unwrap();
    page.settle(Duration::from_millis(50));

    assert_eq!(
        page.eval_to_string("window.__frames").unwrap(),
        "0",
        "the previous document's animation frame must not fire after navigation"
    );
}

/// Regression: `clearTimeout` only records the id, leaving the timer in the heap
/// until it comes due. A cleared far-future timer therefore reported a deadline
/// that kept `settle` blocked for its entire budget instead of returning at
/// quiescence.
#[test]
fn a_cleared_far_future_timer_does_not_stall_settle() {
    let page = page(
        "<!DOCTYPE html><body><script>\
           clearTimeout(setTimeout(function(){}, 2000000000));\
         </script></body>",
    );

    let start = Instant::now();
    page.settle(Duration::from_secs(2));
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "settle must return at quiescence, not burn its budget waiting on a \
         cleared timer (took {:?})",
        start.elapsed()
    );
}

/// `performance.timing` milestones are recorded in non-decreasing order through
/// a full load, `unload*`/`redirect*` stay 0, and a second navigation re-stamps
/// `navigationStart`.
#[test]
fn performance_timing_milestones_are_monotonic_and_reset_on_navigation() {
    let page = page("<!DOCTYPE html><html><head><title>t</title></head><body>hi</body></html>");
    let monotonic = "(() => {\
        const t = performance.timing;\
        const seq = [t.navigationStart, t.domLoading, t.domInteractive,\
            t.domContentLoadedEventStart, t.domContentLoadedEventEnd,\
            t.domComplete, t.loadEventStart, t.loadEventEnd];\
        if (!(t.navigationStart > 0)) return false;\
        for (let i = 1; i < seq.length; i++) if (seq[i] < seq[i - 1]) return false;\
        return t.unloadEventStart === 0 && t.redirectEnd === 0\
            && t.secureConnectionStart === 0;\
    })()";
    assert_eq!(page.eval_to_string(monotonic).unwrap(), "true");

    // Remember the first navigationStart, navigate, and confirm the new one was
    // re-stamped (>= the old) — the timing reset on navigation.
    page.eval_to_string("window.__firstNav = performance.timing.navigationStart")
        .unwrap();
    let page = page;
    page.load_html("<!DOCTYPE html><html><head><title>t2</title></head><body>bye</body></html>")
        .unwrap();
    assert_eq!(
        page.eval_to_string("performance.timing.navigationStart >= window.__firstNav")
            .unwrap(),
        "true"
    );
    assert_eq!(page.eval_to_string(monotonic).unwrap(), "true");
}

#[test]
fn a_body_onload_attribute_runs_at_the_window_load_event() {
    // WPT's check-layout suites hang without this: `check-layout-th.js` declares
    // `setup({explicit_done: true})` and calls `done()` only from `checkLayout`,
    // which markup invokes as `<body onload="checkLayout(…)">`. The `load` event
    // fires at the *window*, so the body's handler has to reflect onto it — a
    // handler filed under the body element would never run, and the harness would
    // wait out its timeout with zero subtests.
    let page = page(
        "<!DOCTYPE html><html><body onload=\"globalThis.ran = this === globalThis\"></body></html>",
    );
    assert_eq!(page.eval_to_string("globalThis.ran").unwrap(), "true");
}

#[test]
fn a_body_onload_attribute_sees_the_parsed_document() {
    // The handler runs at `load`, so the tree it measures is complete.
    let page = page(
        "<!DOCTYPE html><html><body onload=\"globalThis.count = document.querySelectorAll('p').length\">\
           <p></p><p></p><p></p>\
         </body></html>",
    );
    assert_eq!(page.eval_to_string("globalThis.count").unwrap(), "3");
}

#[test]
fn a_body_handler_outside_the_window_reflected_set_stays_on_the_element() {
    // Only the window-reflecting handlers (`load`, `error`, `resize`, ...) move
    // to the window; `onclick` remains an ordinary handler on the body element.
    let page = page(
        "<!DOCTYPE html><html><body onclick=\"globalThis.target = this.localName\"></body></html>",
    );
    page.eval("document.body.dispatchEvent(new Event('click'))")
        .unwrap();
    assert_eq!(page.eval_to_string("globalThis.target").unwrap(), "body");
}

#[test]
fn an_idl_event_handler_assignment_actually_fires() {
    // `el.onclick = fn` used to be accepted and read back as a function while
    // never running: an inert expando, since no interface declared the attribute.
    let page = page("<!DOCTYPE html><html><body><div id=d></div></body></html>");
    page.eval(
        "const d = document.getElementById('d');\
         d.onclick = () => { globalThis.ran = true; };\
         d.dispatchEvent(new Event('click'));",
    )
    .unwrap();
    assert_eq!(page.eval_to_string("globalThis.ran").unwrap(), "true");
}

#[test]
fn event_handlers_are_prototype_accessors_so_feature_detection_works() {
    // P6: an absent API must be absent, and a present one must be detectable.
    // `"onclick" in el` was false, and the property was a plain data property.
    let page = page("<!DOCTYPE html><html><body><div id=d></div></body></html>");
    assert_eq!(
        page.eval_to_string(
            "const desc = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'onclick');\
             [('onclick' in document.getElementById('d')),\
              ('onload' in window),\
              typeof desc.get, typeof desc.set].join(',')",
        )
        .unwrap(),
        "true,true,function,function"
    );
}

#[test]
fn a_window_onload_idl_assignment_runs_at_the_load_event() {
    // The other half of `<body onload>`: WPT files that install their harness
    // hook with `window.onload = ...` sat in TIMEOUT because the assignment was
    // silently inert.
    let page = page(
        "<!DOCTYPE html><html><body><script>\
           window.onload = function () { globalThis.ran = this === globalThis; };\
         </script></body></html>",
    );
    assert_eq!(page.eval_to_string("globalThis.ran").unwrap(), "true");
}

#[test]
fn a_window_reflected_idl_handler_on_the_body_addresses_the_window() {
    // `document.body.onload = fn` installs the *Window's* load handler: `load` is
    // fired at the window and never at the body, so the two must be one slot.
    let page = page(
        "<!DOCTYPE html><html><body><script>\
           document.body.onload = function () { globalThis.ran = true; };\
           globalThis.same = document.body.onload === window.onload;\
         </script></body></html>",
    );
    assert_eq!(page.eval_to_string("globalThis.same").unwrap(), "true");
    assert_eq!(page.eval_to_string("globalThis.ran").unwrap(), "true");
}

#[test]
fn assigning_a_non_function_clears_an_event_handler() {
    let page = page("<!DOCTYPE html><html><body><div id=d></div></body></html>");
    page.eval(
        "const d = document.getElementById('d');\
         d.onclick = () => { globalThis.count = (globalThis.count || 0) + 1; };\
         d.dispatchEvent(new Event('click'));\
         d.onclick = null;\
         d.dispatchEvent(new Event('click'));",
    )
    .unwrap();
    assert_eq!(page.eval_to_string("globalThis.count").unwrap(), "1");
    assert_eq!(
        page.eval_to_string("document.getElementById('d').onclick")
            .unwrap(),
        "null"
    );
}

#[test]
fn an_idl_assignment_supersedes_the_content_attribute_until_it_is_edited() {
    // Spec ordering: the assignment replaces the compiled content attribute, but
    // a *later* edit of that attribute wins back.
    let page = page(
        "<!DOCTYPE html><html><body>\
           <div id=d onclick=\"globalThis.from = 'attribute'\"></div>\
         </body></html>",
    );
    page.eval(
        "document.getElementById('d').onclick = () => { globalThis.from = 'idl'; };\
         document.getElementById('d').dispatchEvent(new Event('click'));",
    )
    .unwrap();
    assert_eq!(page.eval_to_string("globalThis.from").unwrap(), "idl");

    page.eval(
        "document.getElementById('d')\
           .setAttribute('onclick', \"globalThis.from = 'attribute again'\");\
         document.getElementById('d').dispatchEvent(new Event('click'));",
    )
    .unwrap();
    assert_eq!(
        page.eval_to_string("globalThis.from").unwrap(),
        "attribute again"
    );
}
