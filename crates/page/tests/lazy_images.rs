//! Viewport-driven lazy `<img>` loading (ADR-0014): with `lazy_images`, an
//! image is fetched only once it reaches the viewport plus one screen of margin.
//!
//! "Loaded" is judged by the *server's* request counter, not by layout: layout
//! only proves an image arrived, while the point of the feature is that most of
//! them never do. The counter is stable at assert time because `settle` returns
//! only when nothing is in flight.

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

/// Per-image request counters, one per served image URL.
#[derive(Clone, Default)]
struct Hits {
    img: Arc<AtomicUsize>,
    img2: Arc<AtomicUsize>,
}

impl Hits {
    fn img(&self) -> usize {
        self.img.load(Ordering::SeqCst)
    }

    fn img2(&self) -> usize {
        self.img2.load(Ordering::SeqCst)
    }
}

/// A loopback server: `html` at `/index.html`, a second document at `/next.html`,
/// two counted 100×50 PNGs, and a stylesheet that reveals `#hidden`.
fn spawn_server(html: String) -> (u16, Hits) {
    let hits = Hits::default();
    let server_hits = hits.clone();
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
                let hits = server_hits.clone();
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
                    let _ = sock.write_all(&route(&path, &html, &hits)).await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    (rx.recv().unwrap(), hits)
}

fn route(path: &str, html: &str, hits: &Hits) -> Vec<u8> {
    match path {
        "/index.html" => resp(200, "OK", "text/html", html.as_bytes()),
        // The document a navigation lands on: one image, above the fold.
        "/next.html" => resp(
            200,
            "OK",
            "text/html",
            b"<!DOCTYPE html><body style='margin:0'><img src='/img2.png' style='display:block'>",
        ),
        "/img.png" => {
            hits.img.fetch_add(1, Ordering::SeqCst);
            resp(200, "OK", "image/png", &png_bytes(100, 50))
        }
        "/img2.png" => {
            hits.img2.fetch_add(1, Ordering::SeqCst);
            resp(200, "OK", "image/png", &png_bytes(100, 50))
        }
        "/reveal.css" => resp(200, "OK", "text/css", b"#hidden { display: block }"),
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

/// Navigates a page with `lazy_images = lazy` to a document with `body`, waiting
/// for `load` but *not* settling — for asserting on what `load` alone fetched.
fn load(body: &str, lazy: bool) -> (Page, Hits) {
    let html = format!("<!DOCTYPE html><body style='margin:0'>{body}</body>");
    let (port, hits) = spawn_server(html);
    let mut page = Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        lazy_images: lazy,
        ..PageOptions::default()
    })
    .unwrap();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    (page, hits)
}

/// [`load`] plus a settle: the state every "did it fetch?" assertion is made on.
fn run(body: &str, lazy: bool) -> (Page, Hits) {
    let (page, hits) = load(body, lazy);
    page.settle(Duration::from_secs(5));
    (page, hits)
}

fn settle(page: &Page) {
    page.settle(Duration::from_secs(5));
}

fn eval(page: &Page, script: &str) {
    page.eval_to_string(script).unwrap();
}

/// A 3000px spacer pushes what follows well past the viewport (600px) and its
/// one-viewport margin.
const SPACER: &str = "<div style='height:3000px'></div>";

// === The basics ===

#[test]
fn below_the_fold_image_is_not_fetched() {
    let (_page, hits) = run(
        &format!("{SPACER}<img src='/img.png' style='display:block'>"),
        true,
    );
    assert_eq!(hits.img(), 0, "an image 3000px down must not be fetched");
}

#[test]
fn above_the_fold_image_is_fetched() {
    let (_page, hits) = run("<img src='/img.png' style='display:block'>", true);
    assert_eq!(hits.img(), 1);
}

#[test]
fn scrolling_into_view_fetches_the_image() {
    let (page, hits) = run(
        &format!("{SPACER}<img src='/img.png' style='display:block'>"),
        true,
    );
    assert_eq!(hits.img(), 0, "nothing fetched yet");

    eval(&page, "window.scrollTo(0, 3000)");
    settle(&page);

    assert_eq!(hits.img(), 1, "scrolling it into view must fetch it");
}

/// Eager mode ignores `loading="lazy"` outright: an embedder that did not ask
/// for lazy loading gets the whole document, as it always has.
#[test]
fn eager_mode_fetches_everything() {
    let (_page, hits) = run(
        &format!("{SPACER}<img src='/img.png' loading='lazy' style='display:block'>"),
        false,
    );
    assert_eq!(hits.img(), 1);
}

/// `display: none` generates no box, so there is nothing to intersect and
/// nothing to fetch — what a browser does too. Revealing it restyles the
/// document, which reopens the gate.
#[test]
fn display_none_image_is_fetched_once_revealed() {
    let (page, hits) = run("<img id='i' src='/img.png' style='display:none'>", true);
    assert_eq!(hits.img(), 0, "no box, no fetch");

    eval(
        &page,
        "document.getElementById('i').style.display = 'block'",
    );
    settle(&page);

    assert_eq!(hits.img(), 1, "revealed → fetched");
}

/// A `data:` URL decodes inline and costs no network, so deferring it would buy
/// nothing. It must load below the fold too — where the counter cannot see it,
/// so layout answers: a decoded 100×50 image sizes its `auto` block.
#[test]
fn data_url_image_is_never_deferred() {
    let data = format!(
        "data:image/png;base64,{}",
        base64_encode(&png_bytes(100, 50))
    );
    let (page, _hits) = run(
        &format!("{SPACER}<img src='{data}' style='display:block'>"),
        true,
    );
    assert_eq!(
        page.eval_to_string("document.querySelector('img').offsetHeight")
            .unwrap(),
        "50",
        "a data: image below the fold still decodes"
    );
}

/// The margin is one viewport tall, so the next screen down is fetched and the
/// one after it is not. Both images are `position: absolute` — in flow, the
/// first one's arrival would push the second and make the test a race between
/// convergence steps.
#[test]
fn margin_pulls_in_the_next_screen() {
    let (_page, hits) = run(
        "<img src='/img.png' style='position:absolute;top:900px'>\
         <img src='/img2.png' style='position:absolute;top:2000px'>",
        true,
    );
    assert_eq!(
        hits.img(),
        1,
        "900px: inside the 600px viewport + 600 margin"
    );
    assert_eq!(hits.img2(), 0, "2000px: beyond it");
}

/// An `<img>` with no `width`/`height` and no image yet lays out 0×0. A strict
/// intersection test never counts a zero-area rect as overlapping anything, so
/// it would defer such an image forever — and it would never load to gain the
/// size that would undefer it.
#[test]
fn image_without_dimensions_above_the_fold_is_fetched() {
    let (_page, hits) = run("<img src='/img.png'>", true);
    assert_eq!(hits.img(), 1, "a 0×0 box at the top of the viewport counts");
}

/// The escape hatch for full-page output: everything still deferred loads, and
/// the page is eager from there on.
#[test]
fn load_deferred_images_fetches_everything() {
    let (page, hits) = run(
        &format!("{SPACER}<img src='/img.png' style='display:block'>"),
        true,
    );
    assert_eq!(hits.img(), 0, "deferred");

    page.load_deferred_images(Duration::from_secs(5));

    assert_eq!(hits.img(), 1, "full-page output loads the whole document");
}

// === Regressions ===

/// An external sheet lands without a DOM mutation, bumping only `style.version()`
/// — and `PaintStamp` carries the style version of the *last reflow*, not the
/// live one. Gating the scan on the stamp alone leaves the gate shut while the
/// sheet reveals a first-screen image: a hole in the screenshot.
#[test]
fn external_stylesheet_revealing_an_image_reopens_the_gate() {
    let (_page, hits) = run(
        "<style>#hidden { display: none }</style>\
         <link rel='stylesheet' href='/reveal.css'>\
         <img id='hidden' src='/img.png'>",
        true,
    );
    assert_eq!(
        hits.img(),
        1,
        "an image revealed by an external sheet must be fetched"
    );
}

/// Deferred nodes wait in the queue indefinitely, so an SPA that drops an
/// `<img>` leaves a freed `NodeId` behind — and `bounding_client_rect` panics on
/// one. The scan drops freed nodes before it touches geometry.
#[test]
fn removing_a_deferred_image_does_not_panic() {
    let (page, hits) = run(
        &format!("{SPACER}<img id='i' src='/img.png' style='display:block'>"),
        true,
    );
    assert_eq!(hits.img(), 0, "deferred");

    eval(&page, "document.getElementById('i').remove()");
    page.collect_garbage();
    settle(&page);

    assert_eq!(page.eval_to_string("1+1").unwrap(), "2", "page still alive");
    assert_eq!(hits.img(), 0, "a removed image is never fetched");
}

/// Deferral happens *before* `start_image_load_url`, whose first act is to claim
/// the URL in the dedup set. Claim it while deferring and nothing ever fetches
/// it; skip the dedup on the deferred path and it gets fetched twice.
#[test]
fn deferred_image_shares_the_url_dedup() {
    let (page, hits) = run(
        &format!(
            "<img src='/img.png' style='display:block'>{SPACER}<img src='/img.png' style='display:block'>"
        ),
        true,
    );
    assert_eq!(hits.img(), 1, "the visible one is fetched, once");

    eval(&page, "window.scrollTo(0, 3000)");
    settle(&page);

    assert_eq!(hits.img(), 1, "the deferred twin reuses the loaded URL");
}

/// The queue holds nodes of the outgoing document; their ids go stale with it.
#[test]
fn navigation_clears_deferred_images() {
    let (mut page, hits) = run(
        &format!("{SPACER}<img src='/img.png' style='display:block'>"),
        true,
    );
    assert_eq!(hits.img(), 0, "deferred");
    let url = page.eval_to_string("location.href").unwrap();
    let next = url.replace("index.html", "next.html");

    page.navigate(&next, WaitUntil::Load).unwrap();
    settle(&page);

    assert_eq!(
        hits.img(),
        0,
        "the old document's queue is dropped, not run"
    );
    assert_eq!(hits.img2(), 1, "the new document loads normally");
}

/// Deferred images are not in flight, so they must not hold `load` back — which
/// is also what the spec says about lazy images.
#[test]
fn lazy_image_does_not_block_load() {
    let (page, hits) = load(
        &format!("{SPACER}<img src='/img.png' style='display:block'>"),
        true,
    );
    assert!(page.is_loaded(), "`load` fired");
    assert_eq!(hits.img(), 0, "and it did not wait for the deferred image");
}

/// `loading` is a style-owner attribute now, so writing it re-queues the image.
#[test]
fn setting_loading_eager_from_js_undefers() {
    let (page, hits) = run(
        &format!("{SPACER}<img id='i' src='/img.png' style='display:block'>"),
        true,
    );
    assert_eq!(hits.img(), 0, "deferred");

    eval(
        &page,
        "document.getElementById('i').setAttribute('loading', 'eager')",
    );
    settle(&page);

    assert_eq!(hits.img(), 1, "loading=eager loads it where it stands");
}
