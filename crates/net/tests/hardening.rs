//! Regression tests for the net-crate hardening findings: request timeouts,
//! the no-cors / cross-origin-redirect header safelist, the cumulative byte
//! budget, opaque-response blanking, per-hop request charging, CORS redirect
//! tainting, the `Vary` cache skip, and the async `file://` path.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oxidepage_base::NetErrorKind;
use oxidepage_net::{
    CookieJar, Credentials, FetchEngine, NetRequest, RequestMode, ResourcePolicy, ResponseType,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// --- Shared loopback helpers -------------------------------------------------

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

/// Reads one HTTP request head (up to the blank line) as text.
async fn read_head(sock: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The request-target path from a raw request head.
fn request_path(text: &str) -> String {
    text.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned()
}

/// A single request header's value (case-insensitive), if present.
fn header_value(text: &str, name: &str) -> Option<String> {
    text.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_owned())
    })
}

/// Spawns a loopback server that answers every request via `handler`.
async fn spawn_server<F>(handler: F) -> u16
where
    F: Fn(&str) -> Vec<u8> + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                if let Some(text) = read_head(&mut sock).await {
                    let resp = handler(&text);
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.flush().await;
                }
            });
        }
    });
    port
}

fn engine_with(policy: ResourcePolicy) -> FetchEngine {
    FetchEngine::new(Arc::new(policy), Arc::new(Mutex::new(CookieJar::new()))).unwrap()
}

/// A policy that reaches the loopback server but keeps the SSRF filter on
/// otherwise.
fn loopback_policy() -> ResourcePolicy {
    ResourcePolicy {
        allowlist: vec![std::net::Ipv4Addr::LOCALHOST.into()],
        ..ResourcePolicy::default()
    }
}

/// A same-origin GET against the loopback server on `port`.
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

// --- C2: network timeout -----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_times_out_on_a_silent_server() {
    // Accept the connection but never send a byte of response.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let _hold = sock; // keep the connection open, answer nothing
                tokio::time::sleep(Duration::from_secs(60)).await;
            });
        }
    });

    let policy = ResourcePolicy {
        request_timeout: Duration::from_millis(300),
        ..loopback_policy()
    };
    let eng = engine_with(policy);
    let start = Instant::now();
    let err = eng.fetch(get(port, "/")).await.unwrap_err();
    let elapsed = start.elapsed();
    assert_eq!(err.kind, NetErrorKind::Timeout, "detail: {}", err.detail);
    assert!(
        elapsed < Duration::from_secs(5),
        "fetch must time out promptly, took {elapsed:?}"
    );
}

// --- H1: no-cors safelist + cross-origin redirect credential stripping -------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_cors_request_drops_authorization_header() {
    let port = spawn_server(|text| {
        let body = if header_value(text, "authorization").is_some() {
            "auth:present"
        } else {
            "auth:absent"
        };
        http_response(200, "OK", &[], body.as_bytes())
    })
    .await;

    let origin = format!("http://127.0.0.1:{port}");
    let req = NetRequest {
        method: "GET".to_owned(),
        url: format!("{origin}/echo-auth"),
        headers: vec![("Authorization".to_owned(), "Bearer secret".to_owned())],
        body: None,
        credentials: Credentials::SameOrigin,
        mode: RequestMode::NoCors,
        referrer: None,
        initiator_origin: Some(origin),
        bypass_cache: false,
    };
    let out = engine_with(loopback_policy()).fetch(req).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.body),
        "auth:absent",
        "no-cors must not forward Authorization"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_origin_redirect_strips_authorization() {
    // Bind both origins first so each can name the other's port.
    let la = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let lb = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pa = la.local_addr().unwrap().port();
    let pb = lb.local_addr().unwrap().port();
    let origin_a = format!("http://127.0.0.1:{pa}");
    let redirect_target = format!("http://127.0.0.1:{pb}/echo-auth");

    // Server A (initiator origin): same-origin start that 302s to server B.
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = la.accept().await {
            let loc = redirect_target.clone();
            tokio::spawn(async move {
                if read_head(&mut sock).await.is_some() {
                    let resp = http_response(302, "Found", &[("Location", &loc)], b"");
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.flush().await;
                }
            });
        }
    });
    // Server B (different origin): echoes whether it saw Authorization and
    // permits CORS from A so the fetch completes.
    let acao = origin_a.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = lb.accept().await {
            let acao = acao.clone();
            tokio::spawn(async move {
                if let Some(text) = read_head(&mut sock).await {
                    let body = if header_value(&text, "authorization").is_some() {
                        "auth:present"
                    } else {
                        "auth:absent"
                    };
                    let resp = http_response(
                        200,
                        "OK",
                        &[("Access-Control-Allow-Origin", &acao)],
                        body.as_bytes(),
                    );
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.flush().await;
                }
            });
        }
    });

    let req = NetRequest {
        method: "GET".to_owned(),
        url: format!("{origin_a}/start"),
        headers: vec![("Authorization".to_owned(), "Bearer secret".to_owned())],
        body: None,
        credentials: Credentials::Omit,
        mode: RequestMode::Cors,
        referrer: None,
        initiator_origin: Some(origin_a.clone()),
        bypass_cache: false,
    };
    let out = engine_with(loopback_policy()).fetch(req).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.body),
        "auth:absent",
        "Authorization must be stripped on the cross-origin redirect hop"
    );
    assert_eq!(out.head.response_type, ResponseType::Cors);
}

// --- M3: cumulative byte budget ---------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_cannot_exceed_remaining_byte_budget() {
    let port = spawn_server(|text| match request_path(text).as_str() {
        "/small" => http_response(200, "OK", &[], &vec![b'a'; 4000]),
        "/big" => http_response(200, "OK", &[], &vec![b'b'; 10_000]),
        _ => http_response(404, "Not Found", &[], b""),
    })
    .await;

    let policy = ResourcePolicy {
        max_total_bytes: 5000,
        max_response_bytes: 1_000_000,
        ..loopback_policy()
    };
    let eng = engine_with(policy);

    // First response (4000 B) fits; 1000 B remain of the 5000 B page budget.
    let first = eng.fetch(get(port, "/small")).await.unwrap();
    assert_eq!(first.body.len(), 4000);
    assert_eq!(eng.total_charged_bytes(), 4000);

    // Second response (10000 B) far exceeds the 1000 B that remain → rejected
    // by the remaining-budget read cap, never charged.
    let err = eng.fetch(get(port, "/big")).await.unwrap_err();
    assert!(
        matches!(err.kind, NetErrorKind::Decode | NetErrorKind::Blocked),
        "kind {} detail {}",
        err.kind.as_str(),
        err.detail
    );
    assert_eq!(
        eng.total_charged_bytes(),
        4000,
        "an over-budget response must never be charged"
    );
    assert!(eng.total_charged_bytes() <= 5000);
}

/// Regression: the engine never emitted an `Origin` header, so a third-party
/// server could not tell that a cross-origin `POST` was cross-origin — defeating
/// `Origin`-based CSRF defenses — and CORS servers that require the header broke.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn origin_header_is_sent_on_cross_origin_and_unsafe_requests() {
    type SeenRequests = Arc<Mutex<Vec<(String, Option<String>)>>>;
    let seen: SeenRequests = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let port = spawn_server(move |text| {
        recorder
            .lock()
            .unwrap()
            .push((request_path(text), header_value(text, "origin")));
        http_response(200, "OK", &[("access-control-allow-origin", "*")], b"ok")
    })
    .await;

    let eng = engine_with(loopback_policy());

    // A plain same-origin GET subresource: no Origin (browsers omit it).
    eng.fetch(get(port, "/plain")).await.unwrap();

    // A cross-origin POST: Origin names the initiator.
    let mut post = get(port, "/post");
    post.method = "POST".to_owned();
    post.body = Some(b"x".to_vec());
    post.initiator_origin = Some("https://initiator.example".to_owned());
    post.mode = RequestMode::NoCors;
    eng.fetch(post).await.unwrap();

    // A cors-mode GET: Origin is required even for a safe method.
    let mut cors = get(port, "/cors");
    cors.mode = RequestMode::Cors;
    cors.credentials = Credentials::Omit;
    cors.initiator_origin = Some("https://initiator.example".to_owned());
    eng.fetch(cors).await.unwrap();

    let seen = seen.lock().unwrap();
    let origin_for = |path: &str| {
        seen.iter()
            .find(|(p, _)| p == path)
            .unwrap_or_else(|| panic!("no request for {path}"))
            .1
            .clone()
    };
    assert_eq!(
        origin_for("/plain"),
        None,
        "safe same-origin GET sends none"
    );
    assert_eq!(
        origin_for("/post"),
        Some("https://initiator.example".to_owned()),
        "an unsafe method must name its origin"
    );
    assert_eq!(
        origin_for("/cors"),
        Some("https://initiator.example".to_owned()),
        "a cors-mode request must name its origin"
    );
}

/// Regression: `read_cap` used to be computed from an *unreserved*
/// `max_total_bytes - total_bytes`, so concurrent fetches each saw the whole
/// remaining headroom and could buffer `max_response_bytes` apiece — several
/// times the advertised page budget resident at once. The allowance is now
/// claimed before the body streams.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_responses_share_one_byte_budget() {
    let port = spawn_server(|text| match request_path(text).as_str() {
        "/big" => http_response(200, "OK", &[], &vec![b'b'; 4000]),
        _ => http_response(404, "Not Found", &[], b""),
    })
    .await;

    // Four parallel 4000-byte responses against a 5000-byte page budget: at most
    // one can succeed, and the counter must never exceed the budget.
    let policy = ResourcePolicy {
        max_total_bytes: 5000,
        max_response_bytes: 1_000_000,
        max_requests: 100,
        ..loopback_policy()
    };
    let eng = engine_with(policy);

    let mut tasks = Vec::new();
    for _ in 0..4 {
        let eng = eng.clone();
        tasks.push(tokio::spawn(
            async move { eng.fetch(get(port, "/big")).await },
        ));
    }
    let mut ok = 0;
    for task in tasks {
        if task.await.unwrap().is_ok() {
            ok += 1;
        }
    }

    assert_eq!(ok, 1, "only one 4000 B response fits a 5000 B budget");
    assert_eq!(
        eng.total_charged_bytes(),
        4000,
        "refunds must leave exactly the delivered bytes charged"
    );
    assert!(
        eng.total_charged_bytes() <= 5000,
        "the counter must never exceed the budget"
    );
}

/// Regression: a failed response must refund its reservation, or the budget
/// would leak and later requests would be blocked for no reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_response_refunds_its_reservation() {
    let port = spawn_server(|text| match request_path(text).as_str() {
        "/huge" => http_response(200, "OK", &[], &vec![b'x'; 9000]),
        "/small" => http_response(200, "OK", &[], &[b'a'; 100]),
        _ => http_response(404, "Not Found", &[], b""),
    })
    .await;

    let policy = ResourcePolicy {
        max_total_bytes: 5000,
        max_response_bytes: 1_000_000,
        ..loopback_policy()
    };
    let eng = engine_with(policy);

    assert!(eng.fetch(get(port, "/huge")).await.is_err());
    assert_eq!(
        eng.total_charged_bytes(),
        0,
        "a rejected response leaves nothing charged"
    );
    // The budget is intact, so a small response still succeeds.
    let small = eng.fetch(get(port, "/small")).await.unwrap();
    assert_eq!(small.body.len(), 100);
    assert_eq!(eng.total_charged_bytes(), 100);
}

// --- L1: opaque response blanking -------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_origin_no_cors_response_is_blanked() {
    let port = spawn_server(|_text| {
        http_response(
            200,
            "OK",
            &[("X-Secret", "leak"), ("Content-Type", "text/plain")],
            b"opaque-body",
        )
    })
    .await;

    let req = NetRequest {
        method: "GET".to_owned(),
        url: format!("http://127.0.0.1:{port}/resource"),
        headers: Vec::new(),
        body: None,
        credentials: Credentials::Omit,
        mode: RequestMode::NoCors,
        // A different origin than the loopback server → opaque outcome.
        initiator_origin: Some("http://example.com".to_owned()),
        referrer: None,
        bypass_cache: false,
    };
    let out = engine_with(loopback_policy()).fetch(req).await.unwrap();
    assert_eq!(out.head.response_type, ResponseType::Opaque);
    assert_eq!(out.head.status, 0, "opaque status must be blanked");
    assert!(
        out.head.headers.is_empty(),
        "opaque headers must be blanked"
    );
    assert_eq!(
        &out.body[..],
        b"opaque-body",
        "the body is kept for image decode"
    );
}

// --- L4: per-hop request-count budget ---------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirect_chain_charges_request_budget_per_hop() {
    let port = spawn_server(|text| match request_path(text).as_str() {
        "/hop0" => http_response(302, "Found", &[("Location", "/hop1")], b""),
        "/hop1" => http_response(302, "Found", &[("Location", "/hop2")], b""),
        "/hop2" => http_response(200, "OK", &[], b"done"),
        _ => http_response(404, "Not Found", &[], b""),
    })
    .await;

    // The chain is 3 hops; a 2-request budget must block it.
    let tight = ResourcePolicy {
        max_requests: 2,
        ..loopback_policy()
    };
    let err = engine_with(tight)
        .fetch(get(port, "/hop0"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Blocked, "detail: {}", err.detail);

    // A 3-request budget is exactly enough for the 3 hops.
    let ample = ResourcePolicy {
        max_requests: 3,
        ..loopback_policy()
    };
    let out = engine_with(ample).fetch(get(port, "/hop0")).await.unwrap();
    assert_eq!(&out.body[..], b"done");
}

// --- L5: CORS redirect tainting ---------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cors_taint_applies_after_cross_origin_redirect() {
    let la = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let lb = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pa = la.local_addr().unwrap().port();
    let pb = lb.local_addr().unwrap().port();
    let origin_a = format!("http://127.0.0.1:{pa}");
    let to_mid = format!("http://127.0.0.1:{pb}/mid");
    let back_to_a = format!("http://127.0.0.1:{pa}/end");

    // Server A: /start → B/mid (cross-origin), /end → 200 with NO ACAO.
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = la.accept().await {
            let to_mid = to_mid.clone();
            tokio::spawn(async move {
                if let Some(text) = read_head(&mut sock).await {
                    let resp = match request_path(&text).as_str() {
                        "/start" => http_response(302, "Found", &[("Location", &to_mid)], b""),
                        "/end" => http_response(200, "OK", &[], b"secret-a"),
                        _ => http_response(404, "Not Found", &[], b""),
                    };
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.flush().await;
                }
            });
        }
    });
    // Server B: /mid → 302 back to A/end.
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = lb.accept().await {
            let back = back_to_a.clone();
            tokio::spawn(async move {
                if read_head(&mut sock).await.is_some() {
                    let resp = http_response(302, "Found", &[("Location", &back)], b"");
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.flush().await;
                }
            });
        }
    });

    let req = NetRequest {
        method: "GET".to_owned(),
        url: format!("{origin_a}/start"),
        headers: Vec::new(),
        body: None,
        credentials: Credentials::Omit,
        mode: RequestMode::Cors,
        referrer: None,
        initiator_origin: Some(origin_a.clone()),
        bypass_cache: false,
    };
    // The chain crossed origin at B, so the final (same-origin) hop is still
    // CORS-gated; A/end sends no ACAO → the whole response is blocked instead
    // of leaking `secret-a`.
    let err = engine_with(loopback_policy()).fetch(req).await.unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Blocked, "detail: {}", err.detail);
}

// --- Vary cache skip ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vary_response_is_not_served_from_cache() {
    let port = spawn_server(|text| {
        static HITS: AtomicUsize = AtomicUsize::new(0);
        if request_path(text) == "/vary" {
            let n = HITS.fetch_add(1, Ordering::SeqCst) + 1;
            let body = format!("v{n}");
            http_response(
                200,
                "OK",
                &[("Cache-Control", "max-age=60"), ("Vary", "Accept-Language")],
                body.as_bytes(),
            )
        } else {
            http_response(404, "Not Found", &[], b"")
        }
    })
    .await;

    let eng = engine_with(loopback_policy());
    let first = eng.fetch(get(port, "/vary")).await.unwrap();
    let second = eng.fetch(get(port, "/vary")).await.unwrap();
    // A `Vary` response is never cached against the header-less key, so the
    // server is hit both times and the counter advances.
    assert_ne!(
        first.body, second.body,
        "a Vary response must not be served from cache"
    );
}

// --- L2: file:// through the async engine path -------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_url_loads_through_engine() {
    let dir = std::env::temp_dir().join(format!("oxidepage-net-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("page.html");
    std::fs::write(&path, b"<h1>local</h1>").unwrap();
    let url = url::Url::from_file_path(&path).unwrap();

    let policy = ResourcePolicy {
        allow_file: true,
        ..ResourcePolicy::default()
    };
    let req = NetRequest {
        method: "GET".to_owned(),
        url: url.to_string(),
        headers: Vec::new(),
        body: None,
        credentials: Credentials::Omit,
        mode: RequestMode::Navigate,
        referrer: None,
        initiator_origin: None,
        bypass_cache: false,
    };
    let out = engine_with(policy).fetch(req).await.unwrap();
    assert_eq!(out.head.status, 200);
    assert_eq!(&out.body[..], b"<h1>local</h1>");
    std::fs::remove_dir_all(&dir).ok();
}

// --- `data:` is decoded above the scheme gate, but not across a redirect -----

/// A `data:` URL is decoded into a normal 200 response, carrying the MIME type
/// it declared as `Content-Type` so text consumers can find the charset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_url_is_fetched_without_a_scheme_allowlist_entry() {
    let policy = ResourcePolicy::default();
    assert!(
        !policy.scheme_allowed("data"),
        "the allowlist must stay http/https — `data:` is handled above the gate, not by widening it"
    );

    let req = NetRequest {
        method: "GET".to_owned(),
        url: "data:text/javascript;charset=utf-8;base64,ZDIgPSAndHdvJzs%3D".to_owned(),
        headers: Vec::new(),
        body: None,
        credentials: Credentials::Omit,
        mode: RequestMode::NoCors,
        referrer: None,
        initiator_origin: Some("https://example.com".to_owned()),
        bypass_cache: false,
    };
    let out = engine_with(policy).fetch(req).await.unwrap();

    assert_eq!(out.head.status, 200);
    assert_eq!(out.head.response_type, ResponseType::Basic);
    assert!(!out.head.redirected);
    assert_eq!(&out.body[..], b"d2 = 'two';");
    assert_eq!(
        out.head.headers,
        vec![(
            "content-type".to_owned(),
            "text/javascript;charset=utf-8".to_owned()
        )]
    );
}

/// A malformed `data:` URL is a load error, not an empty success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_data_url_is_an_error() {
    let req = NetRequest {
        method: "GET".to_owned(),
        // No comma: the data: URL processor returns failure.
        url: "data:text/javascript".to_owned(),
        headers: Vec::new(),
        body: None,
        credentials: Credentials::Omit,
        mode: RequestMode::NoCors,
        referrer: None,
        initiator_origin: None,
        bypass_cache: false,
    };
    let err = engine_with(ResourcePolicy::default())
        .fetch(req)
        .await
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::InvalidUrl, "detail: {}", err.detail);
}

/// Handling `data:` at the top of the pipeline must not make it reachable
/// *through* a redirect: the redirect loop re-checks the scheme allowlist, and
/// Fetch requires a redirect to a non-HTTP(S) scheme to be a network error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirect_to_a_data_url_stays_blocked() {
    let port = spawn_server(|_text| {
        http_response(
            302,
            "Found",
            &[("Location", "data:text/html,<h1>redirected</h1>")],
            b"",
        )
    })
    .await;

    let err = engine_with(loopback_policy())
        .fetch(get(port, "/redirect"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Blocked, "detail: {}", err.detail);
    assert!(
        err.detail.contains("data"),
        "the error must name the rejected scheme: {}",
        err.detail
    );
}
