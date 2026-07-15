//! WP-K: image loading — HTTP fetch + decode + intrinsic sizing, broken/404
//! placeholders (non-fatal), `data:` URLs, dynamic `src`, and URL dedup.
//!
//! A block `<img>` with `width: auto` uses the decoded image's intrinsic size
//! (CSS 2.2 §10.3.4), so a non-zero `offsetHeight` (== the image height)
//! confirms the image loaded and sized layout; a broken/missing image stays
//! 0-height.

use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use oxidepage_page::{Page, PageOptions, ResourcePolicy, WaitUntil};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A PNG with the given dimensions (solid red).
fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 30, 30, 255]));
    let mut buf = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// A self-contained loopback server: serves `html` at `/index.html`, a 100×50
/// PNG at `/img.png` (counting requests), a corrupt image at `/broken.png`,
/// and 404 elsewhere.
fn spawn_server(html: String) -> (u16, Arc<AtomicUsize>) {
    let counter = Arc::new(AtomicUsize::new(0));
    let server_counter = Arc::clone(&counter);
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
                let counter = Arc::clone(&server_counter);
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
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let _ = sock.write_all(&route(&path, &html, &counter)).await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    (rx.recv().unwrap(), counter)
}

fn route(path: &str, html: &str, img_requests: &AtomicUsize) -> Vec<u8> {
    match path {
        "/index.html" => resp(200, "OK", "text/html", html.as_bytes()),
        "/img.png" => {
            img_requests.fetch_add(1, Ordering::SeqCst);
            resp(200, "OK", "image/png", &png_bytes(100, 50))
        }
        "/broken.png" => resp(200, "OK", "image/png", b"not a real png"),
        "/bg.css" => resp(
            200,
            "OK",
            "text/css",
            b"body { background-image: url(/img.png) }",
        ),
        _ => resp(404, "Not Found", "text/plain", b"nope"),
    }
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

/// Loads a document with `body` over a fresh loopback server; returns the page
/// and the server's `/img.png` request counter.
fn run(body: &str) -> (Page, Arc<AtomicUsize>) {
    let html = format!("<!DOCTYPE html><body style='margin:0'>{body}</body>");
    let (port, counter) = spawn_server(html);
    let mut page = Page::new(PageOptions {
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
    (page, counter)
}

fn offset_height(page: &Page, selector: &str) -> i64 {
    page.eval_to_string(&format!(
        "document.querySelector('{selector}').offsetHeight"
    ))
    .unwrap()
    .parse()
    .unwrap_or(-1)
}

#[test]
fn http_image_loads_and_sizes_layout() {
    let (page, _) = run("<img style='display:block' src='/img.png'>");
    // 100×50 image; `width: auto` block replaced → intrinsic 100×50.
    assert_eq!(offset_height(&page, "img"), 50);
}

/// An `<img>` queued for load and then freed by a subtree replacement leaves a
/// stale id in the image-update queue. The drain must revalidate it, not panic.
///
/// The mutation runs *after* parsing on purpose: the parser holds off
/// `free_detached_tree_if_unpinned`, so a detach during parsing only
/// disconnects the node, while one after it frees the id outright.
#[test]
fn image_freed_before_drain_is_not_fatal() {
    let (page, _) = run("<div id=c></div>");
    page.eval_to_string(
        "const c = document.getElementById('c');\
         c.innerHTML = '<a><img src=\"/img.png\"></a>';\
         c.innerHTML = '<b>replaced</b>';",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("document.getElementById('c').textContent")
            .unwrap(),
        "replaced"
    );
}

#[test]
fn broken_image_is_not_fatal() {
    let (page, _) = run("<img style='display:block' src='/broken.png'>");
    assert_eq!(offset_height(&page, "img"), 0);
    assert_eq!(page.eval_to_string("1+1").unwrap(), "2");
}

#[test]
fn missing_image_404_is_not_fatal() {
    let (page, _) = run("<img style='display:block' src='/nope.png'>");
    assert_eq!(offset_height(&page, "img"), 0);
    assert_eq!(page.eval_to_string("1+1").unwrap(), "2");
}

#[test]
fn img_fires_load_event_when_src_is_set_by_script() {
    // The shape `relayout-image-load.html` uses, and the one that used to hang
    // that whole file: the image fetched, decoded and relaid out correctly, but
    // no `load` event was ever dispatched, so the test's `onload` — the only
    // thing that would have run its assertions — never fired and the file timed
    // out. The event lands after the store insert, so layout already sees the
    // image by the time a listener asks.
    let (page, _) = run("<img id=i style='display:block'>");
    page.eval_to_string(
        "window.fired = 'none';\
         const i = document.getElementById('i');\
         i.addEventListener('load', () => { window.fired = 'load:' + i.offsetHeight; });\
         i.src = '/img.png';",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(page.eval_to_string("window.fired").unwrap(), "load:50");
}

#[test]
fn every_img_sharing_a_src_gets_its_own_load_event() {
    // `requested_images` deduplicates by URL, so the second `<img>` never issues
    // a request of its own — a waiter keyed by request id would sit there
    // forever. Waiters are keyed by URL for exactly this reason, and the counter
    // pins down that the single fetch really is shared.
    let (page, counter) = run("<script>window.log = []</script>\
         <img id=a src='/img.png' onload=\"log.push('a')\">\
         <img id=b src='/img.png' onload=\"log.push('b')\">");
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("log.sort().join(',')").unwrap(),
        "a,b",
        "both elements must be served by the one load that happened"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1, "the URL is fetched once");
}

#[test]
fn missing_image_fires_error_event() {
    let (page, _) = run("<script>window.fired = 'none'</script>\
         <img src='/nope.png' onerror=\"fired = 'error'\" onload=\"fired = 'load'\">");
    page.settle(Duration::from_secs(5));
    assert_eq!(page.eval_to_string("window.fired").unwrap(), "error");
}

#[test]
fn data_url_image_decodes_inline() {
    let data = format!(
        "data:image/png;base64,{}",
        base64_encode(&png_bytes(100, 50))
    );
    let (page, _) = run(&format!("<img style='display:block' src='{data}'>"));
    assert_eq!(offset_height(&page, "img"), 50);
}

#[test]
fn dynamic_src_loads_new_image() {
    let (page, _) = run("<img id=i style='display:block'>");
    assert_eq!(offset_height(&page, "img"), 0, "no src → 0 height");
    page.eval_to_string("document.getElementById('i').setAttribute('src','/img.png')")
        .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(offset_height(&page, "img"), 50, "after setting src");
}

#[test]
fn duplicate_urls_are_fetched_once() {
    let (_page, counter) =
        run("<img style='display:block' src='/img.png'><img style='display:block' src='/img.png'>");
    assert_eq!(counter.load(Ordering::SeqCst), 1, "same URL fetched once");
}

/// Regression: a `background-image` declared in an **external** stylesheet must
/// be loaded. The `<link>` bumps `dom.style_version()` while the sheet is still
/// in flight; when it lands it bumps only `style.version()`. Gating the scan on
/// the dom counter alone (as it once did) meant the rescan never ran and the
/// image was never fetched — the same trap `@font-face` was already fixed for.
#[test]
fn background_image_from_external_stylesheet_loads() {
    let (_page, counter) = run("<link rel='stylesheet' href='/bg.css'><div>x</div>");
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "background-image from an external sheet must be fetched"
    );
}

/// Regression: the same rescan gate must survive a CSSOM `insertRule`, which
/// also bumps only `style.version()`.
#[test]
fn background_image_from_cssom_insert_rule_loads() {
    let (page, counter) = run("<style id='s'></style><div>x</div>");
    assert_eq!(counter.load(Ordering::SeqCst), 0, "nothing loaded yet");

    page.eval_to_string(
        "document.styleSheets[0].insertRule('body { background-image: url(/img.png) }', 0)",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "background-image inserted via CSSOM must be fetched"
    );
}

/// Regression: a class toggle restyles an element without touching the sheets,
/// bumping only `dom.style_version()`. The gate must fire on that counter too,
/// so switching the gate to `style.version()` alone is equally wrong.
#[test]
fn background_image_revealed_by_a_class_toggle_loads() {
    let (page, counter) =
        run("<style>.on{background-image:url(/img.png)}</style><div id='d'></div>");
    assert_eq!(counter.load(Ordering::SeqCst), 0, "nothing loaded yet");

    page.eval_to_string("document.getElementById('d').className = 'on'")
        .unwrap();
    page.settle(Duration::from_secs(5));

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "background-image revealed by a class change must be fetched"
    );
}

/// Regression: an inline `<svg>` is keyed in the image store by its
/// `outer_html`. A style-only attribute write (here a `class` toggle) changes
/// that markup — and thus the key — without bumping `structure_version`, so
/// gating the rasterization scan on `structure_version` alone left the new key
/// un-rasterized: the next box rebuild's `image_data()` lookup missed the store
/// and the `<svg>` painted a gray placeholder instead of its pixels (and, with
/// no attribute size, would collapse to 0×0). The gate must fire on
/// `style_version` too.
#[test]
fn inline_svg_rerasterizes_after_style_only_attribute_change() {
    // `.big { display: block }` changes the <svg>'s `display`, forcing a full
    // box rebuild (not an in-place patch) so `image_data()` re-runs with the
    // new markup key.
    let body = "<style>.big{display:block}</style>\
                <svg id='s' width='40' height='30'>\
                <rect width='40' height='30' fill='red'/></svg>";
    let (page, _) = run(body);
    assert!(
        page.display_list()
            .to_json()
            .contains("\"type\": \"Image\""),
        "inline <svg> should rasterize and paint an Image on first layout"
    );

    // A class toggle: changes the <svg>'s outer_html (new key) and forces a
    // box rebuild, but does not bump structure_version.
    page.eval_to_string("document.getElementById('s').className = 'big'")
        .unwrap();
    page.settle(Duration::from_secs(5));

    assert!(
        page.display_list()
            .to_json()
            .contains("\"type\": \"Image\""),
        "the <svg> must re-rasterize under its new key after a style-only class \
         change, not fall back to a placeholder"
    );
}

/// Regression: a `background-image` on a `::before` pseudo-element must be
/// scanned and loaded (not only element primary styles), so it actually paints.
#[test]
fn pseudo_element_background_image_loads_and_paints() {
    let data = format!("data:image/png;base64,{}", base64_encode(&png_bytes(4, 4)));
    let body = format!(
        "<style>.b::before{{content:'x';display:block;width:20px;height:20px;\
         background-image:url({data})}}</style><div class='b'></div>"
    );
    let (page, _) = run(&body);
    let json = page.display_list().to_json();
    assert!(
        json.contains("\"type\": \"Image\""),
        "::before background-image should load and paint an Image item;\n{json}"
    );
}
