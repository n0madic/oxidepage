//! Page-level integration tests: document loading with inline scripts,
//! lifecycle events, timers, and the microtask checkpoint.

use std::time::Duration;

use oxidepage_bindings::ConsoleLevel;
use oxidepage_js::JsValue;
use oxidepage_page::{
    NavigatorProfile, Page, PageOptions, ScreenProfile, ScriptErrorKind, load_html_page,
};

fn eval_string(page: &Page, source: &str) -> String {
    page.eval_to_string(source).expect("eval")
}

#[test]
fn blank_page_evaluates_scripts() {
    let page = Page::new(PageOptions::default()).unwrap();
    assert_eq!(eval_string(&page, "1 + 1"), "2");
    assert_eq!(eval_string(&page, "document.URL"), "about:blank");
    assert_eq!(eval_string(&page, "location.href"), "about:blank");
}

/// A Location without the URL-decomposition attributes hangs real pages:
/// `adsbygoogle.js` walks `window.parent` until a frame reports a non-empty
/// `location.hostname`, which never terminates on the top-level window.
#[test]
fn location_exposes_url_decomposition_attributes() {
    let page = Page::new(PageOptions {
        url: Some("https://example.com:8443/a/b?q=1&r=2#frag".to_owned()),
        ..PageOptions::default()
    })
    .unwrap();

    assert_eq!(
        eval_string(
            &page,
            "[location.href, location.origin, location.protocol, location.host,
              location.hostname, location.port, location.pathname, location.search,
              location.hash, String(document.location === window.location)].join('|')"
        ),
        "https://example.com:8443/a/b?q=1&r=2#frag|https://example.com:8443|https:|\
         example.com:8443|example.com|8443|/a/b|?q=1&r=2|#frag|true"
    );
}

/// The top-level window is its own `parent`/`top`, so a frame walk that stops
/// at a window with a hostname must terminate on the first iteration.
#[test]
fn parent_frame_walk_terminates_on_top_level_window() {
    let page = Page::new(PageOptions {
        url: Some("https://example.com/".to_owned()),
        ..PageOptions::default()
    })
    .unwrap();

    assert_eq!(
        eval_string(
            &page,
            "(function () {
               var steps = 0;
               for (var w = window; w; w = w.parent) {
                 if (++steps > 5) return 'runaway';
                 if (w.location && w.location.hostname) return w.location.hostname;
               }
               return 'no-hostname';
             })()"
        ),
        "example.com"
    );
    assert_eq!(
        eval_string(
            &page,
            "[window.top === window, window.parent === window,
                            window.frames === window].join('|')"
        ),
        "true|true|true"
    );
}

#[test]
fn custom_navigator_profile_is_visible_to_script() {
    let page = Page::new(PageOptions {
        navigator: NavigatorProfile {
            user_agent: "Mozilla/5.0 TestHarness/7".to_owned(),
            vendor: "Test Vendor".to_owned(),
            platform: "TestOS".to_owned(),
            languages: vec!["uk-UA".to_owned(), "en-US".to_owned()],
            hardware_concurrency: 3,
            webdriver: true,
            max_touch_points: 4,
        },
        ..PageOptions::default()
    })
    .unwrap();

    assert_eq!(
        eval_string(
            &page,
            "[navigator.userAgent, navigator.vendor, navigator.platform,
              navigator.languages.join(','), navigator.hardwareConcurrency,
              navigator.webdriver, navigator.maxTouchPoints].join('|')"
        ),
        "Mozilla/5.0 TestHarness/7|Test Vendor|TestOS|uk-UA,en-US|3|true|4"
    );
}

#[test]
fn custom_screen_profile_is_visible_and_validated() {
    let page = Page::new(PageOptions {
        screen: Some(ScreenProfile {
            width: 1920,
            height: 1080,
            avail_width: 1900,
            avail_height: 1040,
            color_depth: 30,
            pixel_depth: 30,
        }),
        ..PageOptions::default()
    })
    .unwrap();
    assert_eq!(
        eval_string(
            &page,
            "[screen.width, screen.height, screen.availWidth, screen.availHeight,
              screen.colorDepth, screen.pixelDepth].join('|')"
        ),
        "1920|1080|1900|1040|30|30"
    );

    for invalid in [
        ScreenProfile {
            width: 0,
            height: 600,
            avail_width: 1,
            avail_height: 1,
            color_depth: 24,
            pixel_depth: 24,
        },
        ScreenProfile {
            width: 800,
            height: 600,
            avail_width: 801,
            avail_height: 600,
            color_depth: 24,
            pixel_depth: 24,
        },
        ScreenProfile {
            width: 800,
            height: 600,
            avail_width: 800,
            avail_height: 600,
            color_depth: 0,
            pixel_depth: 24,
        },
    ] {
        assert!(
            Page::new(PageOptions {
                screen: Some(invalid),
                ..PageOptions::default()
            })
            .is_err()
        );
    }
}

#[test]
fn media_query_list_notifies_when_viewport_match_changes() {
    let page = Page::new(PageOptions::default()).unwrap();
    page.eval(
        "globalThis.mediaChanges = 0;
         globalThis.legacyChanges = 0;
         globalThis.mql = matchMedia('(min-width: 900px)');
         mql.onchange = event => { if (event.type === 'change') mediaChanges++; };
         mql.addListener(() => legacyChanges++);",
    )
    .unwrap();
    assert_eq!(eval_string(&page, "mql.matches"), "false");
    page.set_viewport(oxidepage_page::Viewport {
        width: 1000.0,
        height: 600.0,
        dpr: 1.0,
    });
    assert_eq!(
        eval_string(
            &page,
            "[mql.matches, mediaChanges, legacyChanges].join('|')"
        ),
        "true|1|1"
    );
}

/// `MediaQueryList` is a plain EventTarget: `addEventListener('change')` and
/// `removeEventListener` work alongside the legacy aliases, and a query whose
/// result does not flip delivers nothing.
#[test]
fn media_query_list_supports_event_target_listeners() {
    let page = Page::new(PageOptions::default()).unwrap();
    page.eval(
        "globalThis.kept = 0;
         globalThis.removed = 0;
         globalThis.stable = 0;
         globalThis.mql = matchMedia('(min-width: 900px)');
         globalThis.always = matchMedia('all');
         always.addEventListener('change', () => stable++);
         globalThis.drop = () => removed++;
         mql.addEventListener('change', () => kept++);
         mql.addEventListener('change', drop);
         mql.removeEventListener('change', drop);",
    )
    .unwrap();
    page.set_viewport(oxidepage_page::Viewport {
        width: 1000.0,
        height: 600.0,
        dpr: 1.0,
    });
    assert_eq!(
        eval_string(&page, "[kept, removed, stable, always.matches].join('|')"),
        "1|0|0|true"
    );
}

/// The media evaluator behind `matchMedia` is the stylesheet device, so the
/// queries pages actually write agree with the configured viewport, DPR, and
/// color scheme.
#[test]
fn match_media_answers_common_device_queries() {
    let page = Page::new(PageOptions {
        viewport: Some(oxidepage_page::Viewport {
            width: 1280.0,
            height: 720.0,
            dpr: 2.0,
        }),
        ..PageOptions::default()
    })
    .unwrap();
    assert_eq!(
        eval_string(
            &page,
            "[matchMedia('(min-width: 1280px)').matches,
              matchMedia('(max-width: 1279px)').matches,
              matchMedia('(width: 1280px)').matches,
              matchMedia('(min-height: 720px)').matches,
              matchMedia('(max-height: 719px)').matches,
              matchMedia('(orientation: landscape)').matches,
              matchMedia('(orientation: portrait)').matches,
              matchMedia('(device-width: 1280px)').matches,
              matchMedia('(device-height: 720px)').matches,
              matchMedia('(resolution: 2dppx)').matches,
              matchMedia('(min-resolution: 192dpi)').matches,
              matchMedia('(-webkit-device-pixel-ratio: 2)').matches,
              matchMedia('(prefers-color-scheme: light)').matches,
              matchMedia('(prefers-color-scheme: dark)').matches,
              matchMedia('screen').matches,
              matchMedia('print').matches].join('|')"
        ),
        "true|false|true|true|false|true|false|true|true|true|true|true|true|false|true|false"
    );
}

/// A headless page has no pointing device, so the interaction media features
/// answer `none` rather than pretending a mouse is present.
#[test]
fn interaction_media_features_report_no_pointing_device() {
    let page = Page::new(PageOptions::default()).unwrap();
    assert_eq!(
        eval_string(
            &page,
            "[matchMedia('(pointer: none)').matches,
              matchMedia('(pointer: fine)').matches,
              matchMedia('(any-pointer: coarse)').matches,
              matchMedia('(hover: none)').matches,
              matchMedia('(any-hover: hover)').matches].join('|')"
        ),
        "true|false|false|true|false"
    );
}

/// `matchMedia` and `@media` in a stylesheet answer through the same device, so
/// a viewport-dependent query agrees in both paths.
#[test]
fn stylesheet_media_queries_agree_with_match_media() {
    let page = load_html_page(
        "<style>p { color: rgb(1, 1, 1) }
         @media (min-height: 1px) { p { color: rgb(2, 2, 2) } }
         @media (orientation: landscape) { p { color: rgb(3, 3, 3) } }</style>
         <p id='p'>x</p>",
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(
        eval_string(
            &page,
            "getComputedStyle(document.getElementById('p')).color"
        ),
        "rgb(3, 3, 3)"
    );
    assert_eq!(
        eval_string(
            &page,
            "[matchMedia('(min-height: 1px)').matches,
              matchMedia('(orientation: landscape)').matches].join('|')"
        ),
        "true|true"
    );
}

#[test]
fn invalid_navigator_profiles_fail_page_construction() {
    let invalid = [
        NavigatorProfile {
            user_agent: "bad\r\nInjected: yes".to_owned(),
            ..NavigatorProfile::default()
        },
        NavigatorProfile {
            user_agent: "bad\tvalue".to_owned(),
            ..NavigatorProfile::default()
        },
        NavigatorProfile {
            user_agent: String::new(),
            ..NavigatorProfile::default()
        },
        NavigatorProfile {
            languages: Vec::new(),
            ..NavigatorProfile::default()
        },
        NavigatorProfile {
            languages: vec!["bad_language".to_owned()],
            ..NavigatorProfile::default()
        },
        NavigatorProfile {
            hardware_concurrency: 0,
            ..NavigatorProfile::default()
        },
    ];
    for profile in invalid {
        assert!(
            Page::new(PageOptions {
                navigator: profile,
                ..PageOptions::default()
            })
            .is_err()
        );
    }
}

#[test]
fn navigator_identity_survives_same_realm_navigation() {
    let page = load_html_page(
        "<script>globalThis.savedNavigator = navigator;
         globalThis.savedScreen = screen; globalThis.savedPerformance = performance;</script>",
        PageOptions::default(),
    )
    .unwrap();
    page.load_html("<!doctype html><title>second</title>")
        .unwrap();
    assert_eq!(
        eval_string(
            &page,
            "savedNavigator === navigator && navigator === clientInformation &&
             savedScreen === screen && savedPerformance === performance"
        ),
        "true"
    );
}

/// Navigation replaces the arena wholesale. Without carrying the generation
/// high-water mark across, a fresh arena hands out `(k, FIRST_GENERATION)`
/// again — so a `NodeId` a script saved from the *old* document would still
/// pass its generation check and silently resolve to an *unrelated node of the
/// new document*: `saved.textContent` would read a stranger, and GC of the old
/// wrapper would unpin a live node. The old ids must die, not alias.
///
/// The document (slot 0, gen 1) is the deliberate exception: `window.document`
/// is a non-configurable data property whose wrapper outlives the realm-wide
/// navigation, so its payload must keep resolving — to the incoming document.
#[test]
fn old_node_ids_go_stale_across_navigation() {
    let page = load_html_page(
        "<!doctype html><html><body><p id=old>old</p>\
         <script>globalThis.saved = document.getElementById('old');\
                 globalThis.savedBody = document.body;\
                 globalThis.savedDocument = document;</script></body></html>",
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(eval_string(&page, "saved.textContent"), "old");

    page.load_html("<!doctype html><html><body><div id=new>new</div></body></html>")
        .unwrap();

    // The new document parsed, and `document` still names it.
    assert_eq!(
        eval_string(&page, "document.getElementById('new').id"),
        "new"
    );
    assert_eq!(eval_string(&page, "savedDocument === document"), "true");

    // Nodes of the replaced document are gone. The engine has no tree for them
    // to be detached *into* (navigation drops the whole arena), so touching one
    // is a stale-id InvalidStateError — never a read of the new document.
    let probe = |expr: &str| {
        eval_string(
            &page,
            &format!(
                "(() => {{ try {{ return String({expr}); }} catch (e) {{ return e.name; }} }})()"
            ),
        )
    };
    assert_eq!(probe("saved.textContent"), "InvalidStateError");
    assert_eq!(probe("saved.isConnected"), "InvalidStateError");
    assert_eq!(probe("savedBody.tagName"), "InvalidStateError");
    assert_eq!(probe("document.body.contains(saved)"), "InvalidStateError");
}

#[test]
fn inline_scripts_run_during_parse_in_document_order() {
    let page = load_html_page(
        r#"<!DOCTYPE html>
        <html><body>
          <script>globalThis.order = ['first'];</script>
          <div id="mid">between</div>
          <script>
            // The parser has produced everything before this script,
            // and nothing after it.
            order.push(document.getElementById('mid') !== null);
            order.push(document.getElementById('later') === null);
          </script>
          <div id="later"></div>
          <script>order.push(document.getElementById('later') !== null);</script>
        </body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(
        eval_string(&page, "order.join(',')"),
        "first,true,true,true"
    );
    assert!(page.drain_errors().is_empty());
}

#[test]
fn parser_time_document_write_inserts_before_remaining_input_and_runs_scripts() {
    let page = load_html_page(
        r#"<!doctype html><body>
        <div id="before"></div>
        <script id="writer">
          document.write('<span id="written">written</span>');
          document.writeln('<em id="line">line</em>');
          document.write('<script id="written-script">globalThis.writtenCurrent = document.currentScript.id;<\/script>');
        </script>
        <div id="after"></div>
        </body>"#,
        PageOptions::default(),
    )
    .unwrap();

    assert_eq!(
        eval_string(
            &page,
            "Array.from(document.body.children).map(e => e.id).join('|')"
        ),
        "before|writer|written|line|written-script|after"
    );
    assert_eq!(eval_string(&page, "writtenCurrent"), "written-script");
    assert_eq!(
        eval_string(&page, "document.currentScript === null"),
        "true"
    );

    page.eval("document.write('<b id=late>late</b>')").unwrap();
    assert_eq!(
        eval_string(&page, "document.getElementById('late') === null"),
        "true"
    );
}

/// A parser script that writes without bound is stopped by the per-document
/// write budget: the throw ends that script, the parse resumes, and the DOM is
/// left intact.
#[test]
fn parser_time_document_write_budget_stops_a_runaway_script() {
    let page = load_html_page(
        r#"<!doctype html><body>
        <script>
          globalThis.writeCalls = 0;
          try {
            for (;;) { document.write('<i></i>'); writeCalls++; }
          } catch (error) {
            globalThis.writeError = error.name + ': ' + error.message;
          }
        </script>
        <div id="after"></div>
        </body>"#,
        PageOptions::default(),
    )
    .unwrap();

    assert_eq!(
        eval_string(&page, "writeError"),
        "RangeError: document.write budget exceeded"
    );
    assert_eq!(eval_string(&page, "writeCalls"), "1024");
    assert_eq!(
        eval_string(&page, "document.getElementById('after') !== null"),
        "true"
    );
    assert!(page.drain_errors().is_empty());
}

#[test]
fn document_current_script_tracks_inline_classic_execution() {
    let page = load_html_page(
        r#"<html><body>
          <script id="active">
            globalThis.currentDuring = document.currentScript && document.currentScript.id;
            setTimeout(() => {
              globalThis.currentAfter = document.currentScript === null;
            }, 0);
          </script>
        </body></html>"#,
        PageOptions::default(),
    )
    .unwrap();

    assert_eq!(eval_string(&page, "currentDuring"), "active");
    assert_eq!(eval_string(&page, "currentAfter"), "true");
}

#[test]
fn dynamically_connected_inline_script_executes_once_with_current_script() {
    let page = load_html_page(
        "<html><body><div id='first'></div><div id='second'></div></body></html>",
        PageOptions::default(),
    )
    .unwrap();

    page.eval(
        "globalThis.dynamicRuns = 0;
         const script = document.createElement('script');
         script.id = 'dynamic-inline';
         script.text = `dynamicRuns += 1;
             globalThis.dynamicCurrent = document.currentScript && document.currentScript.id;`;
         document.getElementById('first').appendChild(script);",
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(eval_string(&page, "dynamicRuns"), "1");
    assert_eq!(eval_string(&page, "dynamicCurrent"), "dynamic-inline");

    page.eval(
        "(() => {
           const moved = document.getElementById('dynamic-inline');
           document.getElementById('second').appendChild(moved);
           moved.text = 'dynamicRuns += 100';
         })();",
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(eval_string(&page, "dynamicRuns"), "1");
    assert_eq!(
        eval_string(&page, "document.currentScript === null"),
        "true"
    );

    page.eval(
        "(() => {
           const clone = document.getElementById('dynamic-inline').cloneNode(true);
           clone.id = 'dynamic-clone';
           clone.text = `dynamicRuns += 1;
             globalThis.cloneCurrent = document.currentScript && document.currentScript.id;`;
           document.body.appendChild(clone);
         })();",
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(eval_string(&page, "dynamicRuns"), "2");
    assert_eq!(eval_string(&page, "cloneCurrent"), "dynamic-clone");
}

#[test]
/// "Prepare the script element" ends with *immediately execute the script
/// block* for a script-inserted inline classic script, so `appendChild` runs it
/// before it returns — a later `remove()` cannot cancel it, and the script runs
/// exactly once even if the element is reconnected.
fn dynamic_inline_script_executes_during_the_insertion_that_connects_it() {
    let page = load_html_page("<html><body></body></html>", PageOptions::default()).unwrap();
    page.eval(
        "globalThis.runs = 0;
         globalThis.ranBeforeAppendReturned = false;
         globalThis.pendingScript = document.createElement('script');
         pendingScript.text = 'runs += 1';
         document.body.appendChild(pendingScript);
         ranBeforeAppendReturned = runs === 1;
         pendingScript.remove();",
    )
    .unwrap();
    assert_eq!(eval_string(&page, "ranBeforeAppendReturned"), "true");
    page.run_until_stalled();
    assert_eq!(eval_string(&page, "runs"), "1");

    page.eval("document.body.appendChild(pendingScript)")
        .unwrap();
    page.run_until_stalled();
    assert_eq!(eval_string(&page, "runs"), "1");

    // A script that is never connected never runs.
    page.eval(
        "const orphan = document.createElement('script');
         orphan.text = 'runs += 100';
         document.createElement('div').appendChild(orphan);",
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(eval_string(&page, "runs"), "1");
}

/// A script inserted by a script that was itself inserted by a script runs at
/// its own insertion point, so the outer script observes its effects.
#[test]
fn nested_dynamic_inline_scripts_run_innermost_first() {
    let page = load_html_page("<html><body></body></html>", PageOptions::default()).unwrap();
    page.eval(
        "globalThis.order = [];
         const outer = document.createElement('script');
         outer.text = `order.push('outer-start');
           const inner = document.createElement('script');
           inner.text = \"order.push('inner')\";
           document.body.appendChild(inner);
           order.push('outer-saw-inner:' + order.includes('inner'));`;
         document.body.appendChild(outer);
         order.push('caller');",
    )
    .unwrap();
    assert_eq!(
        eval_string(&page, "order.join(',')"),
        "outer-start,inner,outer-saw-inner:true,caller"
    );
}

/// Rocket Loader and similar loaders shim `document.write`, insert a script,
/// and restore the native `write` once the insertion returns. That contract
/// only holds if the inserted inline script executes inside `appendChild`.
#[test]
fn a_write_shim_around_a_dynamic_inline_script_observes_its_output() {
    let page = load_html_page("<html><body></body></html>", PageOptions::default()).unwrap();
    page.eval(
        "globalThis.buffer = '';
         const nativeWrite = document.write;
         document.write = function (text) { buffer += text; };
         const script = document.createElement('script');
         script.text = 'document.write(\"written\")';
         document.body.appendChild(script);
         document.write = nativeWrite;",
    )
    .unwrap();
    assert_eq!(eval_string(&page, "buffer"), "written");
    assert!(page.drain_console().is_empty());
}

#[test]
fn fragment_parsed_scripts_are_inert() {
    let page = load_html_page("<html><body></body></html>", PageOptions::default()).unwrap();
    page.eval(
        "globalThis.fragmentRuns = 0;
         document.body.innerHTML = '<script>fragmentRuns += 1<\\/script>';",
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(eval_string(&page, "fragmentRuns"), "0");
}

#[test]
fn generic_loader_can_copy_attributes_rewrite_type_and_insert_replacement() {
    let page = load_html_page(
        r#"<html><body>
          <script id="protected" type="x-text/javascript" data-token="kept">
            globalThis.loaderRuns = (globalThis.loaderRuns || 0) + 1;
            globalThis.loaderCurrent = document.currentScript && document.currentScript.id;
          </script>
          <script id="activator">
            globalThis.activatorCurrent = document.currentScript.id;
            globalThis.navigatorFallback =
              navigator.userAgent.includes('OxidePage/') && navigator.vendor === '';
            const source = document.getElementById('protected');
            const replacement = document.createElement('script');
            for (const attribute of Array.from(source.attributes)) {
              replacement.setAttribute(attribute.name, attribute.value);
            }
            replacement.id = 'replacement';
            replacement.type = replacement.type.substr(2);
            replacement.text = source.textContent;
            source.parentNode.insertBefore(replacement, source);
          </script>
          <script>globalThis.followingSawLoaderRuns = globalThis.loaderRuns;</script>
        </body></html>"#,
        PageOptions::default(),
    )
    .unwrap();

    assert_eq!(eval_string(&page, "activatorCurrent"), "activator");
    assert_eq!(eval_string(&page, "navigatorFallback"), "true");
    assert_eq!(eval_string(&page, "loaderRuns"), "1");
    assert_eq!(eval_string(&page, "loaderCurrent"), "replacement");
    assert_eq!(eval_string(&page, "followingSawLoaderRuns"), "1");
    assert_eq!(
        eval_string(
            &page,
            "document.getElementById('replacement').getAttribute('data-token')"
        ),
        "kept"
    );
}

#[test]
fn lifecycle_events_fire_in_order() {
    let page = load_html_page(
        r#"<html><body><script>
            globalThis.events = [];
            document.addEventListener('DOMContentLoaded', e => {
                events.push('dcl:' + (e.target === document) + ':' + e.isTrusted);
            });
            window.addEventListener('load', e => {
                events.push('load:' + (e.target === window));
            });
        </script></body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(
        eval_string(&page, "events.join(',')"),
        "dcl:true:true,load:true"
    );
    assert!(page.is_loaded());
}

/// `document.readyState` steps `loading` → `interactive` → `complete`, and the
/// lifecycle events see the value they are supposed to: an inline script runs
/// mid-parse, `DOMContentLoaded` after `domInteractive`, `load` after
/// `domComplete`.
#[test]
fn ready_state_advances_with_the_document_lifecycle() {
    let page = load_html_page(
        r#"<html><body><script>
            globalThis.seen = [document.readyState];
            document.addEventListener('DOMContentLoaded', () => {
                seen.push('dcl:' + document.readyState);
            });
            window.addEventListener('load', () => {
                seen.push('load:' + document.readyState);
            });
        </script></body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(
        eval_string(&page, "seen.join(',')"),
        "loading,dcl:interactive,load:complete"
    );
    assert_eq!(eval_string(&page, "document.readyState"), "complete");
}

/// Each transition fires `readystatechange` on the document, which does not
/// bubble, and the handler reads the *new* state (never the one it left).
#[test]
fn readystatechange_fires_on_every_transition() {
    let page = load_html_page(
        r#"<html><body><script>
            globalThis.states = [];
            globalThis.bubbled = false;
            document.addEventListener('readystatechange', e => {
                states.push(document.readyState + ':' + e.bubbles + ':' + e.isTrusted);
            });
            window.addEventListener('readystatechange', () => { bubbled = true });
        </script></body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(
        eval_string(&page, "states.join(',')"),
        "interactive:false:true,complete:false:true"
    );
    assert_eq!(
        eval_string(&page, "String(bubbled)"),
        "false",
        "readystatechange must not bubble to the window"
    );
}

/// The `onreadystatechange` IDL attribute, alongside `addEventListener`.
#[test]
fn onreadystatechange_handler_attribute_works() {
    let page = load_html_page(
        r#"<html><body><script>
            globalThis.states = [];
            document.onreadystatechange = () => { states.push(document.readyState) };
            globalThis.isFn = typeof document.onreadystatechange === 'function';
        </script></body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(
        eval_string(&page, "states.join(',')"),
        "interactive,complete"
    );
    assert_eq!(eval_string(&page, "String(isFn)"), "true");

    // Assigning a non-function clears it, per the IDL.
    page.eval_to_string("document.onreadystatechange = null")
        .unwrap();
    assert_eq!(
        eval_string(&page, "String(document.onreadystatechange)"),
        "null"
    );
}

/// A navigation restarts the readiness cycle: the new document's parse-time
/// script must not see the old document's `complete`.
#[test]
fn navigation_resets_ready_state_to_loading() {
    let page = load_html_page("<html><body>first</body></html>", PageOptions::default()).unwrap();
    assert_eq!(eval_string(&page, "document.readyState"), "complete");

    page.load_html(
        r#"<html><body><script>globalThis.atParse = document.readyState</script></body></html>"#,
    )
    .unwrap();

    assert_eq!(eval_string(&page, "atParse"), "loading");
    assert_eq!(eval_string(&page, "document.readyState"), "complete");
}

#[test]
fn timers_fire_in_deadline_order() {
    let page = load_html_page(
        r#"<html><body><script>
            globalThis.fired = [];
            setTimeout(() => fired.push('b'), 20);
            setTimeout(() => fired.push('a'), 5);
            setTimeout((x, y) => fired.push('args:' + x + y), 5, 4, 2);
            const dead = setTimeout(() => fired.push('never'), 5);
            clearTimeout(dead);
        </script></body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    page.settle(Duration::from_secs(2));
    assert_eq!(eval_string(&page, "fired.join(',')"), "a,args:42,b");
}

#[test]
fn intervals_repeat_until_cleared() {
    let page = load_html_page(
        r#"<html><body><script>
            globalThis.ticks = 0;
            const id = setInterval(() => {
                ticks += 1;
                if (ticks === 3) clearInterval(id);
            }, 1);
        </script></body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    page.settle(Duration::from_secs(2));
    assert_eq!(eval_string(&page, "ticks"), "3");
}

#[test]
fn zero_delay_timers_run_after_parse_via_run_until_stalled() {
    let page = load_html_page(
        "<html><body><script>globalThis.x = 0; setTimeout(() => { x = 1; }, 0);</script></body></html>",
        PageOptions::default(),
    )
    .unwrap();
    // load_html already runs the loop to quiescence.
    assert_eq!(eval_string(&page, "x"), "1");
}

#[test]
fn microtasks_run_between_script_and_timers() {
    let page = load_html_page(
        r#"<html><body><script>
            globalThis.seq = [];
            setTimeout(() => seq.push('timer'), 0);
            Promise.resolve().then(() => seq.push('microtask'));
            seq.push('sync');
        </script></body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(eval_string(&page, "seq.join(',')"), "sync,microtask,timer");
}

#[test]
fn inline_module_scripts_execute_and_non_js_is_ignored() {
    // Phase 3: inline classic scripts run during parse; inline modules run
    // (deferred) after parse, before DOMContentLoaded; non-JS types are
    // ignored. No network is involved (no `src`).
    let page = load_html_page(
        r#"<html><body>
            <script>globalThis.ran = true;</script>
            <script type="module">globalThis.mod = 1;</script>
            <script type="text/template">not js</script>
            <script type="module">globalThis.mod += 1;</script>
        </body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(eval_string(&page, "ran"), "true");
    // Both inline modules executed, in order.
    assert_eq!(eval_string(&page, "typeof globalThis.mod"), "number");
    assert_eq!(eval_string(&page, "globalThis.mod"), "2");
    assert!(page.drain_errors().is_empty());
}

#[test]
fn script_errors_are_reported_and_do_not_stop_the_parse() {
    let page = load_html_page(
        r#"<html><body>
            <script>throw new Error('boom in script');</script>
            <div id="after"></div>
            <script>globalThis.after = document.getElementById('after') !== null;</script>
        </body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(eval_string(&page, "after"), "true");
    let errors = page.drain_errors();
    assert!(
        errors.iter().any(|e| e.kind == ScriptErrorKind::Uncaught
            && e.name.as_deref() == Some("Error")
            && e.message == "boom in script"),
        "got {errors:?}"
    );
    // The throw site is data now, not a blob glued onto the message.
    assert!(errors[0].location().is_some(), "got {errors:?}");
}

/// A promise that rejects with nobody listening is how a broken page fails
/// silently — the app's bootstrap chain rejects, nothing mounts, and no error
/// ever reaches the embedder. It must be reported. A rejection that *is*
/// handled, even late, must not be.
#[test]
fn unhandled_promise_rejections_are_reported() {
    let page = load_html_page(
        r#"<html><body>
            <script>
              Promise.reject(new Error('nobody catches me'));
              // Handled, but only on a later turn: not an error.
              const late = Promise.reject(new Error('caught later'));
              setTimeout(() => late.catch(() => { globalThis.caught = true; }), 0);
            </script>
        </body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    page.settle(Duration::from_secs(1));
    assert_eq!(eval_string(&page, "globalThis.caught"), "true");

    let errors = page.drain_errors();
    assert!(
        errors
            .iter()
            .any(|e| e.kind == ScriptErrorKind::UnhandledRejection
                && e.message == "nobody catches me"),
        "expected the unhandled rejection to be reported, got {errors:?}"
    );
    assert!(
        !errors.iter().any(|e| e.message.contains("caught later")),
        "a rejection handled on a later turn is not an error, got {errors:?}"
    );
}

/// A runaway script must not wedge the event loop: the budget aborts it, the
/// parse continues, and the abort is reported as such.
#[test]
fn runaway_script_is_aborted_by_the_execution_budget() {
    let started = std::time::Instant::now();
    let page = load_html_page(
        r#"<html><body>
            <script>for (;;) {}</script>
            <div id="after"></div>
            <script>globalThis.after = document.getElementById('after') !== null;</script>
        </body></html>"#,
        PageOptions {
            script_budget: Some(Duration::from_millis(50)),
            ..PageOptions::default()
        },
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(eval_string(&page, "after"), "true");
    let errors = page.drain_errors();
    assert!(
        errors
            .iter()
            .any(|e| e.kind == ScriptErrorKind::ScriptBudget
                && e.message.contains("50 ms execution budget")),
        "got {errors:?}"
    );
}

/// The budget is per task, not per page: each timer callback starts fresh, so
/// a page that stays busy across many short tasks is never aborted.
#[test]
fn script_budget_is_rearmed_for_each_task() {
    let page = load_html_page(
        "<script>
           globalThis.ticks = 0;
           var id = setInterval(function () {
             var until = Date.now() + 12;
             while (Date.now() < until) {}
             if (++ticks === 4) clearInterval(id);
           }, 0);
         </script>",
        PageOptions {
            script_budget: Some(Duration::from_millis(60)),
            ..PageOptions::default()
        },
    )
    .unwrap();
    page.settle(Duration::from_secs(2));
    assert_eq!(eval_string(&page, "ticks"), "4");
    assert!(page.drain_errors().is_empty());
}

#[test]
fn console_output_is_captured() {
    let page = load_html_page(
        "<html><body><script>console.log('from', 'page');</script></body></html>",
        PageOptions::default(),
    )
    .unwrap();
    let console = page.drain_console();
    assert!(
        console
            .iter()
            .any(|m| m.level == ConsoleLevel::Log && m.message == "from page")
    );
}

#[test]
fn document_url_option_is_visible() {
    let page = Page::new(PageOptions {
        url: Some("file:///tmp/index.html".into()),
        ..PageOptions::default()
    })
    .unwrap();
    page.load_html("<html><body></body></html>").unwrap();
    assert_eq!(eval_string(&page, "document.URL"), "file:///tmp/index.html");
    assert_eq!(
        eval_string(&page, "document.baseURI"),
        "file:///tmp/index.html"
    );
}

#[test]
fn dom_state_survives_between_eval_calls() {
    let page = load_html_page(
        "<html><body><div id='d'>x</div></body></html>",
        PageOptions::default(),
    )
    .unwrap();
    page.eval("document.getElementById('d').setAttribute('data-n', '41')")
        .unwrap();
    assert_eq!(
        eval_string(
            &page,
            "+document.getElementById('d').getAttribute('data-n') + 1"
        ),
        "42"
    );
}

#[test]
fn mutation_observer_fires_across_page_checkpoints() {
    let page = load_html_page(
        r#"<html><body><div id="watch"></div><script>
            globalThis.notified = false;
            new MutationObserver(() => { notified = true; })
                .observe(document.getElementById('watch'), { childList: true });
            document.getElementById('watch').appendChild(document.createElement('i'));
        </script></body></html>"#,
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(eval_string(&page, "notified"), "true");
}

/// The node-wrapper cache is keyed by arena *index*, and a freed slot is reused
/// by the next allocation. A `MutationRecord` keeps its target as a bare
/// `NodeId` with no pin, so once that node is gone the id is stale. Resolving it
/// must fail loudly (`InvalidStateError`) — never hand back whichever unrelated
/// node now occupies the slot.
#[test]
fn a_stale_mutation_record_target_never_resolves_to_another_node() {
    let page = load_html_page(
        "<!DOCTYPE html><html><body></body></html>",
        PageOptions::default(),
    )
    .unwrap();
    page.eval_to_string(
        "window.recs = [];\
         new MutationObserver(function(rs){ for (var i=0;i<rs.length;i++) window.recs.push(rs[i]); })\
             .observe(document.body, {childList: true, subtree: true});\
         (function(){\
             var tmp = document.createElement('div');\
             document.body.appendChild(tmp);\
             tmp.appendChild(document.createElement('i'));\
             document.body.removeChild(tmp);\
         })();\
         'ok'",
    )
    .unwrap();
    page.run_until_stalled();

    // Drop the detached subtree, then refill the arena so its slots are reused.
    page.collect_garbage();
    page.run_until_stalled();
    page.eval_to_string(
        "for (var i = 0; i < 4; i++) { document.body.appendChild(document.createElement('b')); } 'ok'",
    )
    .unwrap();

    // One record targets the detached (now freed) <div>; the rest target <body>.
    let targets = page
        .eval_to_string(
            "window.recs.map(function(r){\
                 try { return r.target.nodeName } catch (e) { return 'throw:' + e.name }\
             }).join(',')",
        )
        .unwrap();
    assert!(
        targets.contains("throw:InvalidStateError"),
        "a freed target must raise InvalidStateError, got: {targets}"
    );
    for name in targets.split(',') {
        assert!(
            name == "BODY" || name.starts_with("throw:"),
            "a stale target resolved to a live node ({name}); full: {targets}"
        );
    }
}

/// The mirror of the case above, for the one node-valued event member that
/// *must* survive: `relatedTarget` holds the node's wrapper, and a wrapper pins
/// its node. Held as a bare `NodeId` instead, a detached related target the GC
/// collected left the id naming a freed slot — and dispatch feeds it straight
/// to the shadow-DOM retargeting walk, which reads it with `DomTree::node` and
/// **panics** out of a JS host call.
#[test]
fn a_related_target_is_pinned_by_the_event_that_names_it() {
    let page = load_html_page(
        "<!DOCTYPE html><html><body><div id=here></div></body></html>",
        PageOptions::default(),
    )
    .unwrap();
    // The only reference to the detached <b> is the event's relatedTarget.
    page.eval_to_string(
        "window.ev = (function () {\
             var away = document.createElement('b');\
             away.id = 'away';\
             return new MouseEvent('mouseover', { relatedTarget: away, bubbles: true });\
         })(); 'ok'",
    )
    .unwrap();
    page.collect_garbage();
    page.run_until_stalled();
    // Refill the arena so a freed slot would be reused by an unrelated node.
    page.eval_to_string(
        "for (var i = 0; i < 8; i++) { document.body.appendChild(document.createElement('span')); } 'ok'",
    )
    .unwrap();
    page.collect_garbage();

    // Dispatching must neither panic nor lose the related target.
    assert_eq!(
        eval_string(
            &page,
            "(function () {\
                 var seen = null;\
                 document.getElementById('here')\
                     .addEventListener('mouseover', function (e) { seen = e.relatedTarget; });\
                 document.getElementById('here').dispatchEvent(window.ev);\
                 return seen === null ? 'null' : seen.id + ':' + (seen === window.ev.relatedTarget);\
             })()"
        ),
        "away:true",
        "the related target survives GC and keeps its identity"
    );
}

#[test]
fn eval_returns_engine_values() {
    let page = Page::new(PageOptions::default()).unwrap();
    assert!(matches!(page.eval("({a: 1})").unwrap(), JsValue::Object(_)));
    assert!(page.eval("throw new Error('x')").is_err());
    // Page remains usable after an eval error.
    assert_eq!(eval_string(&page, "'ok'"), "ok");
}

/// `template.content` exposes the template contents fragment to script.
///
/// The parser has always put a `<template>`'s children into a separate fragment
/// rather than its child list — that is why `template.childNodes` is empty while
/// `innerHTML` still serialises them — but the `content` IDL attribute was never
/// installed, so script could not reach the fragment at all. Everything built on
/// `<template>` (Alpine's `x-if`/`x-for`, Lit, hand-rolled web components) clones
/// `content`, and got `undefined`.
#[test]
fn template_content_is_a_document_fragment() {
    let page = load_html_page(
        r#"<!DOCTYPE html><body><template id="t"><li class="row">hello</li></template></body>"#,
        PageOptions::default(),
    )
    .unwrap();

    assert_eq!(
        eval_string(
            &page,
            "const t = document.getElementById('t');
             [t.content.nodeType,                 // 11 = DocumentFragment
              t.content.childNodes.length,
              t.content.firstElementChild.tagName,
              t.childNodes.length,                // contents are NOT children
              t.content === t.content].join(',')" // [SameObject]
        ),
        "11,1,LI,0,true"
    );
}

/// The point of the fragment: it is inert until cloned into the document.
#[test]
fn template_content_clones_into_the_document() {
    let page = load_html_page(
        r#"<!DOCTYPE html><body><template id="t"><li class="row">hello</li></template>
           <ul id="sink"></ul></body>"#,
        PageOptions::default(),
    )
    .unwrap();

    assert_eq!(
        eval_string(
            &page,
            "const t = document.getElementById('t');
             document.getElementById('sink').appendChild(t.content.cloneNode(true));
             const row = document.querySelector('#sink .row');
             [row.textContent,
              row.isConnected,                       // the clone is live...
              t.content.firstElementChild.isConnected,  // ...the original is not
              t.content.childNodes.length].join(',')" // and stays in the template
        ),
        "hello,true,false,1"
    );
}

/// A template built by script gets its fragment on demand — the parser is not
/// the only way in.
#[test]
fn a_scripted_template_has_content_too() {
    let page = Page::new(PageOptions::default()).unwrap();

    assert_eq!(
        eval_string(
            &page,
            "const t = document.createElement('template');
             t.innerHTML = '<b>x</b>';
             [t.content.nodeType, t.content.firstElementChild.tagName,
              t.childNodes.length].join(',')"
        ),
        "11,B,0"
    );
}
