//! The network observer's contract: every announced request reports **exactly
//! one** terminal outcome (ADR-0030).
//!
//! Both directions matter, and each has cost a driver a hang or a lie. Too few:
//! a request aborted before its headers arrive used to vanish, so the in-flight
//! count a `networkidle` wait watches never reached zero. Too many: an
//! `xhr.abort()` after the response had already landed used to append
//! `loadingFailed` to a request that had reported `loadingFinished`, telling a
//! driver a successful load had failed.

use std::cell::RefCell;
use std::rc::Rc;

use oxidepage_net::{NetRequest, NetService, NetworkEvent, ResourcePolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A loopback server answering everything with a short body.
fn spawn_server() -> u16 {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
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
                tokio::spawn(async move {
                    let mut tmp = [0u8; 2048];
                    let _ = sock.read(&mut tmp).await;
                    let body = "hello";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                });
            }
        });
    });
    rx.recv().expect("server failed to start")
}

/// A service whose observer appends every event to the returned log.
fn service_with_log() -> (NetService, Rc<RefCell<Vec<NetworkEvent>>>) {
    let (service, _events) =
        NetService::new(ResourcePolicy::permissive_localhost()).expect("net service");
    let log: Rc<RefCell<Vec<NetworkEvent>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&log);
    service.set_observer(Some(Rc::new(move |event| sink.borrow_mut().push(event))));
    (service, log)
}

fn terminal_count(log: &RefCell<Vec<NetworkEvent>>) -> usize {
    log.borrow()
        .iter()
        .filter(|event| {
            matches!(
                event,
                NetworkEvent::Finished { .. } | NetworkEvent::Failed { .. }
            )
        })
        .count()
}

#[test]
fn aborting_a_finished_request_reports_nothing_further() {
    let port = spawn_server();
    let (service, log) = service_with_log();

    let (id, outcome) = service
        .fetch_blocking_tracked(NetRequest::navigation(format!("http://127.0.0.1:{port}/a")));
    assert!(outcome.is_ok(), "fixture fetch failed: {outcome:?}");
    assert_eq!(terminal_count(&log), 1, "a completed fetch reports once");

    // `xhr.abort()` and `AbortController` both reach here, and both are
    // reachable *after* the response has landed. The guard used to be "did this
    // request ever record a content type, **or** is an observer installed" —
    // and the second half made the first dead, so any abort at all synthesized
    // a failure.
    service.abort(id);
    assert_eq!(
        terminal_count(&log),
        1,
        "an abort after completion must not append a second terminal event: {:?}",
        log.borrow()
    );
}

#[test]
fn aborting_before_the_response_still_reports_a_terminal_event() {
    let port = spawn_server();
    let (service, log) = service_with_log();

    // Started and immediately cancelled: no headers ever reach the page, which
    // is exactly what `reset_document_state` does to every pending subresource
    // on navigation. Without a terminal event here the driver's in-flight count
    // leaks one per `goto`, forever.
    let id = service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/b"),
        format!("http://127.0.0.1:{port}/"),
    ));
    service.abort(id);

    assert_eq!(terminal_count(&log), 1, "an aborted request reports once");
    assert!(
        matches!(log.borrow().last(), Some(NetworkEvent::Failed { error, .. }) if error == "net::ERR_ABORTED"),
        "expected ERR_ABORTED, got {:?}",
        log.borrow()
    );
}
