//! The SSRF battery (design doc §8, Phase 3 exit criterion): every path an
//! attacker could use to reach an internal address is refused —
//! DNS-rebinding, redirect-to-internal (per-hop revalidation), numeric-literal
//! host forms normalized by the URL parser, IPv4-mapped IPv6, and the cloud
//! metadata address.

use std::sync::{Arc, Mutex};

use oxidepage_base::NetErrorKind;
use oxidepage_net::{CookieJar, Credentials, FetchEngine, NetRequest, RequestMode, ResourcePolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A fetch engine with the default (secure) policy: private hosts blocked.
fn secure_engine() -> FetchEngine {
    FetchEngine::new(
        Arc::new(ResourcePolicy::default()),
        Arc::new(Mutex::new(CookieJar::new())),
    )
    .unwrap()
}

fn get(url: &str) -> NetRequest {
    NetRequest {
        method: "GET".to_owned(),
        url: url.to_owned(),
        headers: Vec::new(),
        body: None,
        credentials: Credentials::Omit,
        mode: RequestMode::NoCors,
        referrer: None,
        initiator_origin: None,
        bypass_cache: false,
        ..NetRequest::default()
    }
}

/// Every internal-target form must be refused with `Blocked`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_targets_are_blocked() {
    let engine = secure_engine();
    let cases = [
        // Direct literals.
        "http://127.0.0.1/",
        "http://10.0.0.1/",
        "http://192.168.1.1/",
        "http://169.254.169.254/latest/meta-data/", // cloud metadata
        // DNS name resolving to loopback (DNS-rebinding shape).
        "http://localhost/",
        // Numeric-literal forms normalized by the URL parser.
        "http://2130706433/", // decimal 127.0.0.1
        "http://0x7f.1/",     // hex-dotted 127.0.0.1
        "http://0177.0.0.1/", // octal 127.0.0.1
        "http://127.1/",      // short form 127.0.0.1
        // IPv4-mapped IPv6 loopback.
        "http://[::ffff:127.0.0.1]/",
        // IPv6 loopback.
        "http://[::1]/",
    ];
    for url in cases {
        let err = engine
            .fetch(get(url))
            .await
            .expect_err(&format!("{url} must be blocked"));
        assert_eq!(
            err.kind,
            NetErrorKind::Blocked,
            "{url} → {} ({})",
            err.kind.as_str(),
            err.detail
        );
    }
}

/// A redirect to an internal address is refused when the connector
/// re-validates the new hop — even though the first hop was allowlisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirect_to_internal_is_revalidated_per_hop() {
    // Loopback server that 302s to the cloud metadata endpoint.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });

    // Allowlist the loopback server, keep the SSRF filter on for everything else.
    let policy = ResourcePolicy {
        allowlist: vec![std::net::Ipv4Addr::LOCALHOST.into()],
        ..ResourcePolicy::default()
    };
    let engine =
        FetchEngine::new(Arc::new(policy), Arc::new(Mutex::new(CookieJar::new()))).unwrap();

    let err = engine
        .fetch(get(&format!("http://127.0.0.1:{port}/")))
        .await
        .expect_err("redirect to metadata must be blocked");
    assert_eq!(err.kind, NetErrorKind::Blocked, "detail: {}", err.detail);
}
