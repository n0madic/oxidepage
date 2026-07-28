//! A loopback server for this crate's tests.
//!
//! CLAUDE.md rules out a shared test *helper crate*, and this is not one: it is
//! an ordinary module compiled into each of this crate's test binaries. A brand
//! new crate should not be born with four copies of the same sixty-line server,
//! and every test here needs the same handful of routes.
//!
//! Routes:
//! - `/set-cookie` — sets `sid=<n>` with a per-server counter, so a test can
//!   tell "the jar was shared" from "both pages set their own".
//! - `/echo-cookie` — echoes the `Cookie:` header, or `none`.
//! - `/cached` — `Cache-Control: max-age=60` over a per-server hit counter, so
//!   an identical body proves the second request never reached the wire.
//! - `/delay/<ms>` — an HTML document served after a delay.
//! - `/uses-cache` — a document whose stylesheet is `/cached`.
//! - anything else — a small HTML document naming the path.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use oxidepage_engine::{BrowserOptions, ResourcePolicy};

/// A running loopback server. Dropping it leaves the thread parked on
/// `accept`; tests are short-lived, and a shutdown channel would buy nothing.
pub struct Server {
    pub port: u16,
    cookie_seq: Arc<AtomicUsize>,
    cache_hits: Arc<AtomicUsize>,
}

impl Server {
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// How many times `/cached` actually reached the wire.
    pub fn cache_hits(&self) -> usize {
        self.cache_hits.load(Ordering::SeqCst)
    }
}

pub fn spawn_server() -> Server {
    let cookie_seq = Arc::new(AtomicUsize::new(0));
    let cache_hits = Arc::new(AtomicUsize::new(0));
    let (port_tx, port_rx) = mpsc::channel();

    let thread_cookie_seq = Arc::clone(&cookie_seq);
    let thread_cache_hits = Arc::clone(&cache_hits);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            port_tx.send(listener.local_addr().unwrap().port()).unwrap();
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                let cookie_seq = Arc::clone(&thread_cookie_seq);
                let cache_hits = Arc::clone(&thread_cache_hits);
                tokio::spawn(handle(sock, cookie_seq, cache_hits));
            }
        });
    });

    Server {
        port: port_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        cookie_seq,
        cache_hits,
    }
}

async fn handle(
    mut sock: tokio::net::TcpStream,
    cookie_seq: Arc<AtomicUsize>,
    cache_hits: Arc<AtomicUsize>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
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
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();
    let cookie = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("cookie:"))
        .map(|l| l[7..].trim().to_owned());

    let (content_type, extra, body) = match path.as_str() {
        "/set-cookie" => {
            let n = cookie_seq.fetch_add(1, Ordering::SeqCst) + 1;
            (
                "text/html",
                vec![format!("Set-Cookie: sid=s{n}; Path=/")],
                "<title>set</title>".to_owned(),
            )
        }
        "/echo-cookie" => (
            "text/html",
            Vec::new(),
            format!(
                "<title>{}</title>",
                cookie.unwrap_or_else(|| "none".to_owned())
            ),
        ),
        "/cached" => {
            let n = cache_hits.fetch_add(1, Ordering::SeqCst) + 1;
            (
                "text/css",
                vec!["Cache-Control: max-age=60".to_owned()],
                format!("body {{ --hit: {n}; }}"),
            )
        }
        p if p.starts_with("/delay/") => {
            let ms: u64 = p.trim_start_matches("/delay/").parse().unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(ms)).await;
            ("text/html", Vec::new(), "<title>delayed</title>".to_owned())
        }
        // Loads `/cached` as a stylesheet, so a page visit exercises the cache.
        "/uses-cache" => (
            "text/html",
            Vec::new(),
            "<link rel=stylesheet href=/cached><title>uses-cache</title>".to_owned(),
        ),
        other => (
            "text/html",
            Vec::new(),
            format!(
                "<title>{}</title><p>hello</p>",
                other.trim_start_matches('/')
            ),
        ),
    };

    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for header in extra {
        response.push_str(&header);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    response.push_str(&body);
    let _ = sock.write_all(response.as_bytes()).await;
    let _ = sock.flush().await;
}

/// Browser options that reach the loopback server while keeping the SSRF
/// filter on for everything else. CI never touches the real internet.
pub fn test_options() -> BrowserOptions {
    BrowserOptions {
        policy: ResourcePolicy::permissive_localhost(),
        command_timeout: Duration::from_secs(20),
        close_timeout: Duration::from_secs(10),
        ..BrowserOptions::default()
    }
}
