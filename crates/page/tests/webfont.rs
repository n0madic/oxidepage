//! WP-D: the `@font-face` load pipeline registers a web font end-to-end.
//!
//! A `data:` WOFF2 `@font-face` (the `test.woff2` asset, whose `A` glyph has a
//! 600/1000-em advance) is decoded inline and registered under its CSS family,
//! so text in that family shapes against it. We observe registration through
//! layout: three `A`s at `font-size: 100px` measure 180px wide (3 × 60px) only
//! when the web font is used — a fallback font would give a different advance.

use std::time::Duration;

use oxidepage_page::{Page, PageOptions, ResourcePolicy, WaitUntil, load_html_page};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// `crates/layout/assets/webfont/test.woff2`, base64 (see that dir's
// PROVENANCE.md). The `A`/`F`/`O` glyphs carry real outlines and known advances.
const WOFF2_BASE64: &str = "d09GMgABAAAAAAHAAAoAAAAAA3wAAAF2AAEAAAAAAAAAAAAAAAAAAAAAAAAAAAAABmAATAqBAIEAATYCJAMUCwwABCAFgVsHLBvAAgAuB2xjOVgnPAAe0iTLJuh5jU3HrnPJ9o/x8DHm3v8zx6N4mqQ1jlBE5DohWpMMsS4lSmVkm6iEShfLREZ0u3uJtH7Wn+LXn97/APXh26sSOV+S9RVWrX71Bk/d5P+NaedrUqChTvrL4zDBME8okS/ghQEtkSjh7JYonUWRZHiz/o0KacWgpMiSAWxLsWivIBjjsSYAWowmCpgRgQNFY9RVpbRoh839F/XzTSgAACVQRwmL2AIUAaSQBZgiCLq9mdBJg4ZpkAJpkCk/6K98r39evTKPt3r0O8pEDQeef6TeaeL34Omm+QcW6wkfNwBA7Nr4/fiON08U4C/8veTIMoM8zxEFCYItpZ2RkVGev6uJH1nNc4Yyy/EbIEq6EiQ7o2wXCEIXAVJFV4CyBfMKokgCYVG/1lLb3nzZwM3eaz2bp/PJuVx2LVf+YdXZqmT9l4DrZW1mLjU3YTs6uInMXd0E5pbudnoTfTIGQIIorbhdtWUEAA==";

/// The same asset as raw bytes, for the HTTP path.
const WOFF2_BYTES: &[u8] = include_bytes!("../../layout/assets/webfont/test.woff2");

/// The bundled Ahem font, used below as the *payload* of an `@font-face` rule.
/// `test.woff2` cannot serve there: its x-height and cap-height are 0 and it has
/// no `0` glyph, so it answers no font-metrics query. Ahem's `0` advance is a
/// full em, which makes `ch` a probe for "which collection answered".
const AHEM_TTF: &[u8] = include_bytes!("../../layout/assets/Ahem.ttf");

/// Minimal base64 encoder, so a font asset can be inlined into a `data:` URL
/// (page has no base64 dependency, and its own `base64_decode` is private).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn width_of(html: &str, selector: &str) -> f64 {
    let page = load_html_page(html, PageOptions::default()).unwrap();
    let js = format!("document.querySelector('{selector}').offsetWidth");
    page.eval_to_string(&js).unwrap().parse().unwrap()
}

/// A loopback server: `/index.html`, a 404 at `/missing.woff2`, the real font at
/// `/real.woff2`, and Ahem at `/ahem.ttf`.
fn spawn_server(html: String) -> u16 {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let html = html.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 2048];
                    loop {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("/");
                    let response = match path {
                        "/index.html" => resp(200, "OK", "text/html", html.as_bytes()),
                        "/real.woff2" => resp(200, "OK", "font/woff2", WOFF2_BYTES),
                        "/ahem.ttf" => resp(200, "OK", "font/ttf", AHEM_TTF),
                        _ => resp(404, "Not Found", "text/plain", b"nope"),
                    };
                    let _ = sock.write_all(&response).await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    rx.recv().unwrap()
}

fn resp(status: u16, reason: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

#[test]
fn data_woff2_font_face_is_registered_and_shaped() {
    let html = format!(
        "<!DOCTYPE html><body style='margin:0'>\
           <style>@font-face {{ font-family: 'Web'; \
             src: url(data:font/woff2;base64,{WOFF2_BASE64}) format('woff2'); }}</style>\
           <span id='t' style='font:100px Web; display:inline-block'>AAA</span>\
         </body>"
    );
    // Three `A`s × 60px advance = 180px, only if the web font resolved.
    let width = width_of(&html, "#t");
    assert!(
        (width - 180.0).abs() < 1.0,
        "web-font advance width used: got {width}, expected 180"
    );
}

#[test]
fn font_face_added_via_cssom_insert_rule_is_loaded() {
    // Regression: a stylesheet mutation that bumps only the style engine's
    // version (CSSOM insertRule) — not dom.style_version() — must still trigger
    // an @font-face scan. Previously the scan gated on dom.style_version() and
    // silently missed such rules (the external `<link>`/`@import` case too).
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0'>\
           <style id='s'></style>\
           <span id='t' style='font:100px Web; display:inline-block'>AAA</span>\
         </body>",
        PageOptions::default(),
    )
    .unwrap();

    // Insert the @font-face after load, via CSSOM (bumps style.version() only).
    let rule = format!(
        "@font-face {{ font-family: 'Web'; src: url(data:font/woff2;base64,{WOFF2_BASE64}) format('woff2'); }}"
    );
    page.eval_to_string(&format!(
        "document.styleSheets[0].insertRule({:?}, 0)",
        rule
    ))
    .unwrap();

    // Drive the loop so the font-face scan runs and the data: font registers.
    page.run_until_stalled();

    let width: f64 = page
        .eval_to_string("document.querySelector('#t').offsetWidth")
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (width - 180.0).abs() < 1.0,
        "CSSOM-inserted web font resolved: got {width}, expected 180"
    );
}

/// Regression: per CSS Fonts §4.3 a `src:` entry that fails to download or parse
/// hands off to the next entry. The engine used to pick only the first supported
/// source and give up on it, so the family never resolved.
#[test]
fn undecodable_font_src_falls_through_to_the_next() {
    // `AAAA` decodes to three zero bytes — a supported *format* hint, but not a
    // font. The real WOFF2 declared after it must be tried.
    let html = format!(
        "<!DOCTYPE html><body style='margin:0'>\
           <style>@font-face {{ font-family: 'Web'; \
             src: url(data:font/woff2;base64,AAAA) format('woff2'), \
                  url(data:font/woff2;base64,{WOFF2_BASE64}) format('woff2'); }}</style>\
           <span id='t' style='font:100px Web; display:inline-block'>AAA</span>\
         </body>"
    );
    let width = width_of(&html, "#t");
    assert!(
        (width - 180.0).abs() < 1.0,
        "the second src must be used after the first fails to parse: got {width}, expected 180"
    );
}

/// Regression: the headline case — the first `src:` 404s, so the second must be
/// fetched. Previously the 404 ended the family's resolution.
#[test]
fn font_src_that_404s_falls_through_to_the_next() {
    let html = "<!DOCTYPE html><body style='margin:0'>\
           <style>@font-face { font-family: 'Web'; \
             src: url(/missing.woff2) format('woff2'), url(/real.woff2) format('woff2'); }</style>\
           <span id='t' style='font:100px Web; display:inline-block'>AAA</span>\
         </body>"
        .to_owned();
    let port = spawn_server(html);

    let page = Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.settle(Duration::from_secs(5));

    let width: f64 = page
        .eval_to_string("document.querySelector('#t').offsetWidth")
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (width - 180.0).abs() < 1.0,
        "the fallback src must be fetched after the first 404s: got {width}, expected 180"
    );
}

/// Regression: navigating rebuilds the style and layout engines, and the style
/// engine's font-metrics provider used to be taken from the *outgoing* layout
/// engine — `font_metrics_factory` captures the collection of the engine it is
/// called on. So `ex`/`ch`/`ic` and `font-size-adjust` resolved against the
/// previous document's fonts, where the new document's `@font-face` faces are
/// never registered.
#[test]
fn font_metrics_resolve_against_the_new_document_after_navigation() {
    let html = format!(
        "<!DOCTYPE html><body style='margin:0'>\
           <style>@font-face {{ font-family: 'WebAhem'; \
             src: url(data:font/ttf;base64,{}) format('truetype'); }}</style>\
           <span id='t' style='font:100px WebAhem; width:1ch; display:inline-block'></span>\
         </body>",
        base64_encode(AHEM_TTF)
    );
    let port = spawn_server(html);

    let page = Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.settle(Duration::from_secs(5));

    // Ahem's `0` advance is a full em, so `1ch` is 100px once the metrics query
    // reaches the collection this document registered its web font into. Against
    // the stale collection the family is unknown, no metrics come back, and `ch`
    // falls back to CSS's 0.5em — 50px.
    let width: f64 = page
        .eval_to_string("document.querySelector('#t').offsetWidth")
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (width - 100.0).abs() < 1.0,
        "`ch` must resolve against the navigated document's own font collection: \
         got {width}, expected 100"
    );
}

/// Regression: a web font arriving over the network lands *after* the document
/// has cascaded, and registering it bumped only the layout engine's fonts
/// version — text re-shaped, but stylo reused the `ComputedValues` it had
/// cached, so `ex`/`ch`/`ic` kept resolving against the font that was absent at
/// first cascade. The load must dirty the cascade too.
#[test]
fn an_http_font_load_recascades_metric_dependent_values() {
    let html = "<!DOCTYPE html><body style='margin:0'>\
           <style>@font-face { font-family: 'WebAhem'; \
             src: url(/ahem.ttf) format('truetype'); }</style>\
           <span id='t' style='font:100px WebAhem; width:1ch; display:inline-block'></span>\
         </body>"
        .to_owned();
    let port = spawn_server(html);

    let page = Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.settle(Duration::from_secs(5));

    // Ahem's `0` advance is a full em → `1ch` at `font-size: 100px` is 100px.
    // Cascaded before the face arrived, `ch` falls back to CSS's 0.5em — 50px.
    let width: f64 = page
        .eval_to_string("document.querySelector('#t').offsetWidth")
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (width - 100.0).abs() < 1.0,
        "a font that loads after the first cascade must re-cascade `ch`: \
         got {width}, expected 100"
    );
}

#[test]
fn missing_font_face_falls_back_without_crashing() {
    // A family with no @font-face still renders (falls back); the point is that
    // the pipeline is inert when there is nothing to load.
    let html = "<!DOCTYPE html><body style='margin:0'>\
                  <span id='t' style='font:100px Ahem; display:inline-block'>AAA</span>\
                </body>";
    // Ahem's glyphs are full-em squares: 3 × 100px = 300px.
    let width = width_of(html, "#t");
    assert!(
        (width - 300.0).abs() < 1.0,
        "Ahem fallback advance used: got {width}, expected 300"
    );
}

// === `document.fonts` (CSS Font Loading, trimmed: `ready`/`status`) ===

#[test]
fn document_fonts_is_cached_as_a_same_object() {
    let page = load_html_page("<!DOCTYPE html><body></body>", PageOptions::default()).unwrap();
    assert_eq!(
        page.eval_to_string("document.fonts === document.fonts")
            .unwrap(),
        "true"
    );
}

#[test]
fn document_fonts_ready_resolves_with_no_font_face_rules() {
    // The motivating WPT case: `document.fonts.ready.then(...)` in a document
    // with no web fonts at all must still resolve — not hang forever, which
    // is what happened before `document.fonts` existed at all.
    let page = load_html_page(
        "<!DOCTYPE html><body>\
           <script>\
             window.__resolved = false;\
             document.fonts.ready.then(\
               fs => { window.__resolved = (fs === document.fonts); });\
           </script>\
         </body>",
        PageOptions::default(),
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(
        page.eval_to_string("window.__resolved").unwrap(),
        "true",
        "ready must resolve with the FontFaceSet itself when nothing was ever loading"
    );
}

#[test]
fn document_fonts_load_resolves_with_a_sequence() {
    // angular.dev calls `document.fonts.load(...)` unconditionally (no feature
    // detection); its absence threw `not a function` mid-render and aborted the
    // app. It must exist, return a promise, and resolve with a (possibly empty)
    // `FontFace` array once fonts settle.
    let page = load_html_page(
        "<!DOCTYPE html><body>\
           <script>\
             window.__loaded = null;\
             const p = document.fonts.load('16px \"Nope\"');\
             window.__isPromise = (p instanceof Promise);\
             p.then(faces => { window.__loaded = Array.isArray(faces); });\
           </script>\
         </body>",
        PageOptions::default(),
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(
        page.eval_to_string("window.__isPromise").unwrap(),
        "true",
        "load() must return a Promise"
    );
    assert_eq!(
        page.eval_to_string("window.__loaded").unwrap(),
        "true",
        "load() must resolve with a FontFace array once fonts settle"
    );
}

/// Regression covering the full `ready`/`status` design: a `ready` read while
/// a real `@font-face` load is still in flight must return a promise that is
/// *not* resolved yet (a naive implementation that only checked
/// `pending_fonts` at read time, ignoring "has parsing finished", could
/// resolve before the font-face rule was even scanned) and must resolve once
/// the load settles — not stay pending forever, and not stay resolved-once
/// while `status` flips back to "loading" on a later load.
#[test]
fn document_fonts_status_and_ready_track_an_in_flight_http_font_load() {
    let html = "<!DOCTYPE html><body style='margin:0'>\
           <style>@font-face { font-family: 'Web'; \
             src: url(/real.woff2) format('woff2'); }</style>\
           <span id='t' style='font:100px Web'>A</span>\
           <script>\
             window.__resolved = false;\
             document.fonts.ready.then(() => { window.__resolved = true; });\
           </script>\
         </body>"
        .to_owned();
    let port = spawn_server(html);

    let page = Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap();
    // `DomContentLoaded`, not `Load`: `Load` already waits out every
    // in-flight subresource (including web fonts), so it would never leave a
    // moment to observe "still loading" — that is exactly what `settle`
    // below drives to completion.
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        oxidepage_page::WaitUntil::DomContentLoaded,
    )
    .unwrap();

    assert_eq!(
        page.eval_to_string("document.fonts.status").unwrap(),
        "loading",
        "the @font-face fetch must have started by DOMContentLoaded"
    );
    assert_eq!(
        page.eval_to_string("window.__resolved").unwrap(),
        "false",
        "ready must not resolve before the in-flight load settles"
    );

    page.settle(Duration::from_secs(5));

    assert_eq!(
        page.eval_to_string("document.fonts.status").unwrap(),
        "loaded"
    );
    assert_eq!(
        page.eval_to_string("window.__resolved").unwrap(),
        "true",
        "ready must resolve once the load settles"
    );

    // A `ready` read *after* settling must hand back an already-resolved
    // promise, not a stale pending one from the earlier read — it resolves
    // on the very next microtask checkpoint, with no further network or
    // event-loop activity needed.
    page.eval_to_string(
        "window.__resettled = false;\
         document.fonts.ready.then(() => { window.__resettled = true; });",
    )
    .unwrap();
    page.run_until_stalled();
    assert_eq!(
        page.eval_to_string("window.__resettled").unwrap(),
        "true",
        "a ready read after settling must resolve, not hang pending forever"
    );
}
