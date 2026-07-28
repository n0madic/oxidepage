//! Stage B verification: the fetch pipeline against a loopback router —
//! redirects (with per-hop SSRF/scheme re-validation), cookie round-trips,
//! and gzip decompression.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use oxidepage_base::NetErrorKind;
use oxidepage_net::{
    CachePartition, CookieJar, Credentials, FetchEngine, NetPool, NetRequest, RequestDefaults,
    RequestMode, ResourcePolicy,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Server-side hit counter for `/cached` (proves a cache hit never reaches it).
static CACHED_HITS: AtomicUsize = AtomicUsize::new(0);

/// A loopback router serving a handful of pipeline probes. Each response
/// closes the connection, so every request re-enters the connector.
async fn spawn_router() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
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
                let text = String::from_utf8_lossy(&buf);
                let path = text
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_owned();
                let cookie = text
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("cookie:"))
                    .map(|l| l[7..].trim().to_owned());

                let response: Vec<u8> = match path.as_str() {
                    "/dest" => http_response(200, "OK", &[], b"arrived"),
                    "/redirect" => http_response(302, "Found", &[("Location", "/dest")], b""),
                    "/redirect-scheme" => {
                        http_response(302, "Found", &[("Location", "ftp://example.com/")], b"")
                    }
                    "/redirect-meta" => http_response(
                        302,
                        "Found",
                        &[("Location", "http://169.254.169.254/latest/")],
                        b"",
                    ),
                    "/setcookie" => {
                        http_response(200, "OK", &[("Set-Cookie", "sid=xyz; Path=/")], b"ok")
                    }
                    "/echo-cookie" => {
                        let body = cookie.unwrap_or_else(|| "none".to_owned());
                        http_response(200, "OK", &[], body.as_bytes())
                    }
                    "/gzip" => {
                        let gz = gzip(b"hello-gzip").await;
                        http_response(200, "OK", &[("Content-Encoding", "gzip")], &gz)
                    }
                    "/cached" => {
                        let n = CACHED_HITS.fetch_add(1, Ordering::SeqCst) + 1;
                        let body = format!("v{n}");
                        http_response(
                            200,
                            "OK",
                            &[("Cache-Control", "max-age=60")],
                            body.as_bytes(),
                        )
                    }
                    "/badenc" => {
                        // A chained encoding the pipeline does not decode: must
                        // surface as an error, never as still-compressed text.
                        let gz = gzip(b"payload").await;
                        http_response(200, "OK", &[("Content-Encoding", "gzip, br")], &gz)
                    }
                    _ => http_response(404, "Not Found", &[], b"nope"),
                };
                let _ = sock.write_all(&response).await;
                let _ = sock.flush().await;
            });
        }
    });
    port
}

fn http_response(status: u16, reason: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    out.push_str("Connection: close\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

async fn gzip(data: &[u8]) -> Vec<u8> {
    use async_compression::tokio::bufread::GzipEncoder;
    let mut enc = GzipEncoder::new(std::io::Cursor::new(data.to_vec()));
    let mut out = Vec::new();
    enc.read_to_end(&mut out).await.unwrap();
    out
}

/// Policy allowing our loopback server while keeping the SSRF filter on for
/// everything else — so a redirect to an internal address is still blocked.
fn engine() -> FetchEngine {
    let policy = ResourcePolicy {
        allowlist: vec![std::net::Ipv4Addr::LOCALHOST.into()],
        ..ResourcePolicy::default()
    };
    FetchEngine::new(Arc::new(policy), Arc::new(Mutex::new(CookieJar::new()))).unwrap()
}

fn get(port: u16, path: &str) -> NetRequest {
    let origin = format!("http://127.0.0.1:{port}");
    NetRequest {
        method: "GET".to_owned(),
        url: format!("{origin}{path}"),
        headers: Vec::new(),
        body: None,
        credentials: Credentials::Include,
        mode: RequestMode::NoCors,
        referrer: None,
        initiator_origin: Some(origin),
        bypass_cache: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirect_is_followed() {
    let port = spawn_router().await;
    let out = engine().fetch(get(port, "/redirect")).await.unwrap();
    assert_eq!(out.head.status, 200);
    assert!(out.head.redirected);
    assert!(out.head.final_url.ends_with("/dest"));
    assert_eq!(&out.body[..], b"arrived");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirect_to_disallowed_scheme_blocked() {
    let port = spawn_router().await;
    let err = engine()
        .fetch(get(port, "/redirect-scheme"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Blocked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirect_to_internal_address_blocked_per_hop() {
    let port = spawn_router().await;
    // The first hop (loopback) is allowlisted; the redirect target
    // (169.254.169.254 metadata) is not — and the connector re-validates it.
    let err = engine()
        .fetch(get(port, "/redirect-meta"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Blocked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cookies_round_trip() {
    let port = spawn_router().await;
    let eng = engine();
    let set = eng.fetch(get(port, "/setcookie")).await.unwrap();
    assert_eq!(set.head.status, 200);
    let echo = eng.fetch(get(port, "/echo-cookie")).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&echo.body),
        "sid=xyz",
        "cookie should have been sent back"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gzip_body_decompressed() {
    let port = spawn_router().await;
    let out = engine().fetch(get(port, "/gzip")).await.unwrap();
    assert_eq!(&out.body[..], b"hello-gzip");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_serves_second_get_without_hitting_server() {
    let port = spawn_router().await;
    let eng = engine();
    // `/cached` is `Cache-Control: max-age=60` and its body embeds a
    // server-side hit counter: a cache hit keeps the body identical.
    let first = eng.fetch(get(port, "/cached")).await.unwrap();
    let second = eng.fetch(get(port, "/cached")).await.unwrap();
    assert_eq!(
        first.body, second.body,
        "second GET must be served from cache (server not re-hit)"
    );
    // `bypass_cache` forces a fresh network hit → a new counter value.
    let mut fresh = get(port, "/cached");
    fresh.bypass_cache = true;
    let third = eng.fetch(fresh).await.unwrap();
    assert_ne!(third.body, first.body, "bypass_cache must re-fetch");
}

/// Two engines over one shared connection pool and cache, in the partitions
/// given — the shape a `Browser` builds for two pages (ADR-0027 D7).
fn shared_engines(a: CachePartition, b: CachePartition) -> (FetchEngine, FetchEngine) {
    let policy = Arc::new(ResourcePolicy {
        allowlist: vec![std::net::Ipv4Addr::LOCALHOST.into()],
        ..ResourcePolicy::default()
    });
    // Through a `NetPool`, which is the only thing that pairs a client with the
    // policy its SSRF connector was built from.
    let pool = NetPool::new(policy).unwrap();
    let build = |partition| {
        FetchEngine::with_shared(
            pool.shared_parts(partition),
            // A jar each: sharing the cache is a browser-level decision, and
            // sharing the jar is a context-level one.
            Arc::new(Mutex::new(CookieJar::new())),
            RequestDefaults::default(),
        )
    };
    (build(a), build(b))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_cache_serves_a_second_engine_in_the_same_partition() {
    let port = spawn_router().await;
    let (one, two) = shared_engines(CachePartition(7), CachePartition(7));
    // `/cached`'s body embeds a server-side hit counter, so an identical body
    // proves the second engine never reached the server.
    let first = one.fetch(get(port, "/cached")).await.unwrap();
    let second = two.fetch(get(port, "/cached")).await.unwrap();
    assert_eq!(
        first.body, second.body,
        "a sibling page in the same partition must be served from the shared cache"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_partitions_do_not_see_each_others_entries() {
    let port = spawn_router().await;
    let (one, two) = shared_engines(CachePartition(1), CachePartition(2));
    let first = one.fetch(get(port, "/cached")).await.unwrap();
    let other = two.fetch(get(port, "/cached")).await.unwrap();
    assert_ne!(
        first.body, other.body,
        "another context's entry must be a miss, not a hit"
    );
    // ... and the second context's own repeat still hits its own partition.
    let again = two.fetch(get(port, "/cached")).await.unwrap();
    assert_eq!(other.body, again.body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chained_content_encoding_is_a_decode_error() {
    let port = spawn_router().await;
    let err = engine().fetch(get(port, "/badenc")).await.unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Decode);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_count_budget_is_enforced() {
    let port = spawn_router().await;
    let policy = ResourcePolicy {
        allowlist: vec![std::net::Ipv4Addr::LOCALHOST.into()],
        max_requests: 2,
        ..ResourcePolicy::default()
    };
    let eng = FetchEngine::new(Arc::new(policy), Arc::new(Mutex::new(CookieJar::new()))).unwrap();
    assert!(eng.fetch(get(port, "/dest")).await.is_ok());
    assert!(eng.fetch(get(port, "/echo-cookie")).await.is_ok());
    // The third request exceeds the per-page budget and is blocked outright.
    let err = eng.fetch(get(port, "/gzip")).await.unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Blocked);
}
