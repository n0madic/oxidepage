//! Stage A verification: a plain loopback GET works through the full
//! connector→TLS-or-plain→client stack, and the SSRF connector refuses a
//! hostname that resolves to a loopback address.

use std::sync::Arc;

use bytes::Bytes;
use http::Request;
use http_body_util::{BodyExt, Full};
use oxidepage_base::NetErrorKind;
use oxidepage_net::{HttpClient, ResourcePolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawns a one-shot loopback HTTP/1.1 server returning `hello`, returns its
/// bound port.
async fn spawn_hello_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        // Serve a couple of connections so a keep-alive probe or retry works.
        for _ in 0..4 {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = b"hello";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.flush().await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_get_works() {
    let port = spawn_hello_server().await;
    // Permissive policy: keeps HTTP(S)-only + budgets but allows loopback.
    let client = HttpClient::new(Arc::new(ResourcePolicy::permissive_localhost())).unwrap();

    let req = Request::builder()
        .uri(format!("http://127.0.0.1:{port}/"))
        .body(Full::<Bytes>::new(Bytes::new()))
        .unwrap();
    let resp = client
        .send_once(req)
        .await
        .expect("loopback GET should work");
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssrf_refuses_name_resolving_to_loopback() {
    let port = spawn_hello_server().await;
    // Default policy blocks private hosts.
    let client = HttpClient::new(Arc::new(ResourcePolicy::default())).unwrap();

    // `localhost` resolves to 127.0.0.1 (and/or ::1) — every resolved address
    // is a loopback address, so the connector must refuse.
    let req = Request::builder()
        .uri(format!("http://localhost:{port}/"))
        .body(Full::<Bytes>::new(Bytes::new()))
        .unwrap();
    let err = client
        .send_once(req)
        .await
        .expect_err("loopback host must be blocked");
    assert_eq!(err.kind(), NetErrorKind::Blocked, "detail: {}", err.detail);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssrf_refuses_ip_literal_loopback() {
    let client = HttpClient::new(Arc::new(ResourcePolicy::default())).unwrap();
    let req = Request::builder()
        .uri("http://127.0.0.1:9/")
        .body(Full::<Bytes>::new(Bytes::new()))
        .unwrap();
    let err = client
        .send_once(req)
        .await
        .expect_err("loopback literal must be blocked");
    assert_eq!(err.kind(), NetErrorKind::Blocked);
}
