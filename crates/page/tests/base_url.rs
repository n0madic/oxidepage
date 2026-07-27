//! `<base href>` drives subresource loading: `Page::resolve_url` resolves
//! against the document base URL, not the document URL, so a relative
//! `<script src>` / `<img src>` under a `<base>` is fetched base-relative.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxidepage_page::{Page, PageOptions, ResourcePolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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

/// A server that records every requested path. Returns `(port, paths)`.
fn spawn_recording_server() -> (u16, Arc<Mutex<Vec<String>>>) {
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
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
                    } else {
                        resp("text/javascript", b"globalThis.__loaded = true;")
                    };
                    let _ = sock.write_all(&body).await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    (rx.recv().unwrap(), paths)
}

#[test]
fn subresources_resolve_against_the_base_href() {
    let (port, paths) = spawn_recording_server();
    let page = Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap();
    page.load_html(&format!(
        r#"<head><base href="http://127.0.0.1:{port}/assets/"></head>
           <body><script src="s.js"></script><img src="p.png"></body>"#
    ))
    .unwrap();

    let mut seen = paths.lock().unwrap().clone();
    seen.sort();
    assert_eq!(
        seen,
        vec!["/assets/p.png".to_owned(), "/assets/s.js".to_owned()],
        "relative subresources must be fetched relative to <base href>"
    );
    // The script really ran, so the resolved URL was fetchable, not just requested.
    assert_eq!(page.eval_to_string("__loaded === true").unwrap(), "true");
}
