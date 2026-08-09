//! A frame's subresources are **its** document's (ADR-0035 D1).
//!
//! Two questions the engine used to answer with the top-level document: *may
//! this node load at all* (the `IS_CONNECTED` gate ADR-0028 D3 spelled as
//! `node_document(el) == dom.document()`) and *what is a relative URL relative
//! to*. Both are per rendered document now, and both are asserted here against
//! what the **server** was asked for — a relative URL resolved against the
//! embedder still fetches *something*, so an assertion on the rendered result
//! passes for the wrong reason.

use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxidepage_page::{Page, PageOptions, ResourcePolicy, WaitUntil, load_html_page};

/// A 7x5 solid-red PNG.
fn png_bytes() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(7, 5, image::Rgba([200, 30, 30, 255]));
    let mut buf = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

fn resp(content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// A server recording every requested path.
///
/// `/index.html` embeds `/nested/frame.html`, which asks for three relative
/// subresources. Resolved against the frame they are `/nested/*`; resolved
/// against the embedder they are `/*` — and the server answers both, which is
/// exactly why the assertion is on the path and not on the result.
fn spawn_server() -> (u16, Arc<Mutex<Vec<String>>>) {
    let paths = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&paths);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
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
                    seen.lock().unwrap().push(path.clone());
                    let body = if path.ends_with(".png") {
                        resp("image/png", &png_bytes())
                    } else if path.ends_with(".css") {
                        resp("text/css", b"#p { color: rgb(1, 2, 3); }")
                    } else if path.ends_with("cp1251.js") {
                        // `document.title = 'привет';` in **windows-1251**.
                        // Read as UTF-8 every one of those bytes becomes
                        // U+FFFD, so the title says so if the charset is lost.
                        let mut src = b"document.title = '".to_vec();
                        src.extend_from_slice(&[0xEF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]);
                        src.extend_from_slice(b"';");
                        resp("text/javascript; charset=windows-1251", &src)
                    } else if path.ends_with(".js") {
                        resp("text/javascript", b"document.title = 'ran';")
                    } else if path == "/charset.html" {
                        resp(
                            "text/html",
                            b"<!doctype html><iframe id=f src='nested/charset-frame.html'></iframe>",
                        )
                    } else if path.ends_with("charset-frame.html") {
                        resp(
                            "text/html",
                            b"<!doctype html><script src='cp1251.js'></script>",
                        )
                    } else if path == "/origin.html" {
                        resp(
                            "text/html",
                            b"<!doctype html><title>Origin</title><iframe id=b></iframe>",
                        )
                    } else if path.ends_with("/frame.html") {
                        resp(
                            "text/html",
                            b"<!doctype html><link rel=stylesheet href='s.css'>\
                              <p id=p>x</p><img id=i src='p.png'><script src='s.js'></script>",
                        )
                    } else {
                        resp(
                            "text/html",
                            b"<!doctype html><title>Top</title>\
                              <link rel=stylesheet href='nested/s.css'><p id=p>y</p>\
                              <iframe id=f src='nested/frame.html'></iframe>",
                        )
                    };
                    let _ = sock.write_all(&body).await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    (rx.recv().unwrap(), paths)
}

fn page() -> Page {
    Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap()
}

#[test]
fn a_frames_relative_subresources_resolve_against_the_frames_document() {
    let (port, paths) = spawn_server();
    let page = page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.settle(Duration::from_millis(1500));

    let mut seen = paths.lock().unwrap().clone();
    seen.sort();
    seen.dedup();
    for expected in ["/nested/s.css", "/nested/p.png", "/nested/s.js"] {
        assert!(
            seen.iter().any(|path| path == expected),
            "the frame did not ask for {expected}; saw {seen:?}"
        );
    }
    // And never the embedder-relative spelling — that is the failure this
    // exists to catch, and the server would have answered it.
    for wrong in ["/s.css", "/p.png", "/s.js"] {
        assert!(
            !seen.iter().any(|path| path == wrong),
            "a frame's relative URL was resolved against its embedder: {seen:?}"
        );
    }

    // The script ran in the frame's own realm, against the frame's document.
    assert_eq!(
        page.eval_to_string("document.getElementById('f').contentDocument.title")
            .unwrap(),
        "ran"
    );
    // The embedder's own `<link>` names the same file, so its sheet applying is
    // not evidence the frame's did — the frame's element is read below.
    assert_eq!(
        page.eval_to_string("getComputedStyle(document.getElementById('p')).color")
            .unwrap(),
        "rgb(1, 2, 3)"
    );
    // The stylesheet applied there too.
    assert_eq!(
        page.eval_to_string(
            "getComputedStyle(document.getElementById('f').contentDocument\
             .getElementById('p')).color"
        )
        .unwrap(),
        "rgb(1, 2, 3)"
    );
}

/// The smallest 1×1 GIF, as a `data:` URL — enough for an `<img>` to settle
/// without a server.
const PIXEL: &str =
    "data:image/gif;base64,R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw==";

/// An `<img>` inside a frame fires `load`.
///
/// The gate read "is this node in *the* rendered document", so the frame's
/// image never reached the loader at all: no `load`, no `error`, and a page
/// waiting on either waited forever. It still *painted*, because the paint
/// path collects image URLs from the layout tree — which is why this asserts
/// the event and not the pixels.
#[test]
fn an_image_in_a_frame_fires_its_load_event() {
    let page = load_html_page(
        &format!(
            "<!DOCTYPE html><body><iframe id='f' srcdoc='\
             <img id=i onload=\"document.title = &#39;loaded&#39;\" src=\"{PIXEL}\">\
             '></iframe></body>"
        ),
        PageOptions::default(),
    )
    .unwrap();
    page.settle(Duration::from_millis(500));
    assert_eq!(
        page.eval_to_string("document.getElementById('f').contentDocument.title")
            .unwrap(),
        "loaded"
    );
}

/// And so does one inside a **shadow tree** — the same gate, the other way it
/// was wrong. A node in a shadow tree is owned by its shadow root, so
/// `node_document` answers with a `DocumentFragment` and the comparison against
/// the document could never hold.
#[test]
fn an_image_in_a_shadow_tree_fires_its_load_event() {
    let page = load_html_page(
        &format!(
            "<!DOCTYPE html><body><div id='host'></div><script>\
             window.loaded = false;\
             const root = document.getElementById('host').attachShadow({{ mode: 'open' }});\
             const img = document.createElement('img');\
             img.addEventListener('load', () => {{ window.loaded = true; }});\
             img.src = '{PIXEL}';\
             root.appendChild(img);\
             </script></body>"
        ),
        PageOptions::default(),
    )
    .unwrap();
    page.settle(Duration::from_millis(500));
    assert_eq!(page.eval_to_string("window.loaded").unwrap(), "true");
}

/// A frame's `<script src>` is decoded with the charset its `Content-Type`
/// declares, like every other script path in the engine.
///
/// `from_utf8_lossy` turned each non-ASCII byte of a legacy-encoded script into
/// U+FFFD, corrupting its string literals — and, when a multi-byte sequence
/// straddled a quote, its syntax.
#[test]
fn a_frames_external_script_honours_its_charset() {
    let (port, _paths) = spawn_server();
    let page = page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/charset.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.settle(Duration::from_millis(1500));

    assert_eq!(
        page.eval_to_string("document.getElementById('f').contentDocument.title")
            .unwrap(),
        "привет"
    );
}

/// A frame with no `src` inherits its embedder's origin, so the embedder can
/// reach into it.
///
/// Every other test masks this: a page loaded from a string has the URL
/// `about:blank` itself, so the same-origin check takes its `a == b` fast path.
/// From a real address the frame's literal `about:blank` compared `about` to
/// `http` and lost — leaving `contentDocument` null for the commonest idiom
/// there is, `createElement('iframe')` + `appendChild` + write.
#[test]
fn a_src_less_frame_inherits_the_embedders_origin() {
    let (port, _paths) = spawn_server();
    let page = page();
    let base = format!("http://127.0.0.1:{port}");
    page.navigate(&format!("{base}/origin.html"), WaitUntil::Load)
        .unwrap();
    page.settle(Duration::from_millis(1000));

    // The one the markup declared…
    assert_eq!(
        page.eval_to_string("document.getElementById('b').contentDocument !== null")
            .unwrap(),
        "true"
    );
    // …and one the page builds itself, which is the idiom that matters. The
    // context is attached by the event loop rather than by `appendChild`
    // (ADR-0035 D5), so the read comes after a turn.
    page.eval_to_string(
        "(() => { const f = document.createElement('iframe'); f.id = 'made'; \
           document.body.appendChild(f); return 0; })()",
    )
    .unwrap();
    page.settle(Duration::from_millis(500));
    assert_eq!(
        page.eval_to_string(
            "(() => { const f = document.getElementById('made'); \
               if (!f.contentDocument) return 'null'; \
               f.contentDocument.body.innerHTML = '<p id=q>in</p>'; \
               return f.contentDocument.getElementById('q').textContent; })()"
        )
        .unwrap(),
        "in"
    );
    // A relative URL inside such a frame resolves against the embedder's base
    // URL, which is the other half of what inheriting the URL buys.
    assert_eq!(
        page.eval_to_string("document.getElementById('b').contentDocument.URL")
            .unwrap(),
        format!("{base}/origin.html")
    );
}
