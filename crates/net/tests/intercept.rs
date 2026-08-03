//! The pause point's contract (ADR-0032 D1–D3, D5, D8, D9).
//!
//! Modelled on `observer.rs`, and for the same reason: the property that
//! matters most is not "does a decision arrive" but **exactly one terminal
//! event per request, whatever resolved it**. A fulfilled request that closed
//! its record out differently from a fetched one would leak the request as
//! in-flight forever and hang every `networkidle` wait — the failure ADR-0030
//! D5 records as having already cost a driver twice.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use oxidepage_net::{
    AuthResponse, FulfilledResponse, InterceptCommand, NetEvent, NetRequest, NetService,
    NetworkEvent, RequestOverrides, RequestPattern, ResourcePolicy, ResourceType,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A loopback server with three behaviours, keyed by path:
///
/// * `/auth` — 401 `WWW-Authenticate: Basic` until an `Authorization` header
///   arrives, then 200 with the credentials it saw.
/// * `/echo` — 200 whose body names the method and the path.
/// * anything else — 200 `hello`.
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
                    let mut tmp = [0u8; 4096];
                    let read = sock.read(&mut tmp).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&tmp[..read]).into_owned();
                    let head = request.lines().next().unwrap_or_default().to_owned();
                    let authorization = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                        .map(|line| line["authorization:".len()..].trim().to_owned());

                    let proxy_authorization = request
                        .lines()
                        .find(|line| {
                            line.to_ascii_lowercase()
                                .starts_with("proxy-authorization:")
                        })
                        .map(|line| line["proxy-authorization:".len()..].trim().to_owned());

                    let response = if head.contains("/always-401") {
                        // Never accepts, whatever it is sent — the shape that
                        // makes an unbounded retry visible.
                        String::from(
                            "HTTP/1.1 401 Unauthorized\r\n\
                             WWW-Authenticate: Basic realm=\"nope\"\r\n\
                             Content-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                    } else if head.contains("/proxy-auth") {
                        // Accepts only `Proxy-Authorization`, so credentials
                        // sent in `Authorization` never get through.
                        match proxy_authorization {
                            Some(value) => ok_response(&format!("proxy-authorized {value}")),
                            None => String::from(
                                "HTTP/1.1 407 Proxy Authentication Required\r\n\
                                 Proxy-Authenticate: Basic realm=\"gateway\"\r\n\
                                 Content-Length: 0\r\nConnection: close\r\n\r\n",
                            ),
                        }
                    } else if head.contains("/auth") {
                        match authorization {
                            Some(value) => ok_response(&format!("authorized {value}")),
                            None => String::from(
                                "HTTP/1.1 401 Unauthorized\r\n\
                                 WWW-Authenticate: Basic realm=\"wonderland\"\r\n\
                                 Content-Length: 0\r\nConnection: close\r\n\r\n",
                            ),
                        }
                    } else if head.contains("/echo") {
                        ok_response(&head)
                    } else {
                        ok_response("hello")
                    };
                    let _ = sock.write_all(response.as_bytes()).await;
                });
            }
        });
    });
    rx.recv().expect("server failed to start")
}

fn ok_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

struct Harness {
    service: NetService,
    events: crossbeam_channel::Receiver<NetEvent>,
    log: Rc<RefCell<Vec<NetworkEvent>>>,
}

impl Harness {
    fn new() -> Self {
        let (service, events) =
            NetService::new(ResourcePolicy::permissive_localhost()).expect("net service");
        let log: Rc<RefCell<Vec<NetworkEvent>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&log);
        service.set_observer(Some(Rc::new(move |event| sink.borrow_mut().push(event))));
        Self {
            service,
            events,
            log,
        }
    }

    /// The id of the one request announced as paused, if any.
    fn paused_id(&self) -> Option<oxidepage_net::RequestId> {
        self.log.borrow().iter().find_map(|event| match event {
            NetworkEvent::Paused { id, .. } => Some(*id),
            _ => None,
        })
    }

    fn terminal_count(&self) -> usize {
        self.log
            .borrow()
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    NetworkEvent::Finished { .. } | NetworkEvent::Failed { .. }
                )
            })
            .count()
    }

    /// Pumps async net events into the service's log until `deadline`, the way
    /// the page's event loop does.
    fn pump(&self, until: impl Fn(&Self) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            self.service.drain_decisions();
            while let Ok(event) = self.events.try_recv() {
                if !self.service.note_event(&event) {
                    self.service.begin_auth_pause(event);
                    continue;
                }
            }
            if until(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out; log: {:?}", self.log.borrow());
    }

    fn body_of(&self, id: oxidepage_net::RequestId) -> String {
        let (bytes, _) = self.service.response_body(id).expect("a retained body");
        String::from_utf8(bytes).expect("utf-8")
    }
}

fn enable_all(harness: &Harness) {
    harness.service.intercept().enable(Vec::new(), false);
}

// === D1: what pauses, and what must never ===

#[test]
fn a_data_url_never_pauses() {
    // The load-bearing half of D1. `fetch_inner` answers `data:` above the
    // scheme gate (ADR-0029) and a driver stores no request record for one, so
    // a `requestPaused` for a `data:` URL is never continued — every inline
    // image, font and module would hang until the intercept timeout.
    let harness = Harness::new();
    enable_all(&harness);

    let outcome = harness
        .service
        .fetch_blocking(NetRequest::navigation("data:text/plain,inline"));

    assert_eq!(outcome.expect("data: decoded").body.as_ref(), b"inline");
    assert!(
        harness.paused_id().is_none(),
        "a data: URL must not pause: {:?}",
        harness.log.borrow()
    );
}

#[test]
fn a_pattern_that_does_not_match_does_not_pause() {
    let port = spawn_server();
    let harness = Harness::new();
    harness.service.intercept().enable(
        vec![RequestPattern {
            url_pattern: String::from("*/never"),
            resource_type: None,
        }],
        false,
    );

    let outcome = harness
        .service
        .fetch_blocking(NetRequest::navigation(format!("http://127.0.0.1:{port}/a")));

    assert_eq!(outcome.expect("fetched").body.as_ref(), b"hello");
    assert!(harness.paused_id().is_none());
}

#[test]
fn a_resource_type_pattern_selects_one_kind_of_request() {
    let port = spawn_server();
    let harness = Harness::new();
    harness.service.intercept().enable(
        vec![RequestPattern {
            url_pattern: String::from("*"),
            resource_type: Some(ResourceType::Image),
        }],
        false,
    );

    // A document request: announced, never paused.
    let outcome = harness
        .service
        .fetch_blocking(NetRequest::navigation(format!("http://127.0.0.1:{port}/a")));
    assert!(outcome.is_ok());
    assert!(harness.paused_id().is_none());

    // An image request: paused, and released by the timeout-free resolution
    // below.
    let id = harness.service.start_resource(
        NetRequest::subresource(
            format!("http://127.0.0.1:{port}/i.png"),
            format!("http://127.0.0.1:{port}/"),
        )
        .of_type(ResourceType::Image),
    );
    assert_eq!(harness.paused_id(), Some(id));
    harness
        .service
        .intercept()
        .send(InterceptCommand::release(id));
    harness.pump(|h| h.terminal_count() == 2);
}

// === D3: the two resolution shapes ===

#[test]
fn a_continued_request_goes_out_under_the_same_id() {
    let port = spawn_server();
    let harness = Harness::new();
    enable_all(&harness);

    let id = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:{port}/"),
    ));
    assert_eq!(harness.paused_id(), Some(id), "the request paused");

    harness
        .service
        .intercept()
        .send(InterceptCommand::release(id));
    harness.pump(|h| h.terminal_count() == 1);

    assert_eq!(harness.body_of(id), "hello");
    // A resumed request must not re-announce itself: a second
    // `requestWillBeSent` for one request is two requests to a driver, and the
    // in-flight count never balances again.
    let requested = harness
        .log
        .borrow()
        .iter()
        .filter(|event| matches!(event, NetworkEvent::Requested { .. }))
        .count();
    assert_eq!(requested, 1, "exactly one announcement per request");
}

#[test]
fn an_override_rewrites_the_url_and_the_method() {
    let port = spawn_server();
    let harness = Harness::new();
    enable_all(&harness);

    let id = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:{port}/"),
    ));
    harness
        .service
        .intercept()
        .send(InterceptCommand::Continue {
            id,
            overrides: Box::new(RequestOverrides {
                url: Some(format!("http://127.0.0.1:{port}/echo")),
                method: Some(String::from("POST")),
                post_data: Some(b"payload".to_vec()),
                ..RequestOverrides::default()
            }),
        });
    harness.pump(|h| h.terminal_count() == 1);

    // The echo route answers with the request line it received, so the body
    // proves both rewrites reached the wire.
    assert!(
        harness.body_of(id).starts_with("POST /echo"),
        "got {:?}",
        harness.body_of(id)
    );
}

#[test]
fn a_fulfilled_request_reports_exactly_one_terminal_event() {
    let port = spawn_server();
    let harness = Harness::new();
    enable_all(&harness);

    let id = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:{port}/"),
    ));
    harness.service.intercept().send(InterceptCommand::Fulfill {
        id,
        response: Box::new(FulfilledResponse {
            status: 201,
            status_text: String::from("Created"),
            headers: vec![(String::from("content-type"), String::from("text/plain"))],
            body: b"stubbed".to_vec(),
        }),
    });
    harness.pump(|h| h.terminal_count() == 1);

    assert_eq!(harness.body_of(id), "stubbed");
    assert_eq!(
        harness.terminal_count(),
        1,
        "a fulfilled request closes out exactly once: {:?}",
        harness.log.borrow()
    );
    let responded = harness
        .log
        .borrow()
        .iter()
        .any(|event| matches!(event, NetworkEvent::Responded { status, .. } if *status == 201));
    assert!(responded, "the fabricated status reached the observer");
}

#[test]
fn a_fulfilled_empty_body_still_closes_its_record() {
    // The `Chunk` is skipped for an empty body, so `Done` is the *only* place a
    // bodyless response closes out. A fulfil path that diverged here would leak
    // the request as in-flight forever and hang every `networkidle` wait.
    let port = spawn_server();
    let harness = Harness::new();
    enable_all(&harness);

    let id = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:{port}/"),
    ));
    harness.service.intercept().send(InterceptCommand::Fulfill {
        id,
        response: Box::new(FulfilledResponse {
            status: 204,
            status_text: String::from("No Content"),
            headers: Vec::new(),
            body: Vec::new(),
        }),
    });
    harness.pump(|h| h.terminal_count() == 1);

    assert_eq!(harness.terminal_count(), 1);
}

#[test]
fn a_failed_request_reports_one_failure() {
    let port = spawn_server();
    let harness = Harness::new();
    enable_all(&harness);

    let id = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:{port}/"),
    ));
    harness.service.intercept().send(InterceptCommand::Fail {
        id,
        error: String::from("net::ERR_FAILED"),
    });
    harness.pump(|h| h.terminal_count() == 1);

    assert!(
        matches!(
            harness.log.borrow().last(),
            Some(NetworkEvent::Failed { error, .. }) if error.contains("net::ERR_FAILED")
        ),
        "got {:?}",
        harness.log.borrow()
    );
}

#[test]
fn a_second_decision_for_one_request_is_refused_before_it_is_sent() {
    let port = spawn_server();
    let harness = Harness::new();
    enable_all(&harness);

    let id = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:{port}/"),
    ));
    let control = harness.service.intercept();
    assert!(control.resolve(InterceptCommand::release(id)));
    // Two sessions both intercepting: the loser must be told, not silently
    // served, or the request goes out twice.
    assert!(!control.resolve(InterceptCommand::release(id)));

    harness.pump(|h| h.terminal_count() == 1);
    assert_eq!(harness.terminal_count(), 1);
}

// === D3 / D7: abort and release ===

#[test]
fn aborting_a_paused_request_releases_it_and_reports_once() {
    let port = spawn_server();
    let harness = Harness::new();
    enable_all(&harness);

    let id = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:{port}/"),
    ));
    assert_eq!(harness.paused_id(), Some(id));

    // `reset_document_state` does this to every pending load on navigation. A
    // parked `NetRequest` left behind would leak one per navigation, and a late
    // `continueRequest` would resurrect a dead document's request into the live
    // one.
    harness.service.abort(id);
    assert_eq!(harness.terminal_count(), 1);
    assert!(
        harness.service.intercept().paused_ids().is_empty(),
        "the shared paused set must not retain an aborted request"
    );

    // The late decision finds nothing and changes nothing.
    harness
        .service
        .intercept()
        .send(InterceptCommand::release(id));
    harness.service.drain_decisions();
    assert_eq!(harness.terminal_count(), 1);
}

#[test]
fn disable_reports_every_paused_id_so_the_caller_can_release_them() {
    let port = spawn_server();
    let harness = Harness::new();
    enable_all(&harness);

    let first = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:{port}/"),
    ));
    let second = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/b"),
        format!("http://127.0.0.1:{port}/"),
    ));

    let released = harness.service.intercept().disable();
    assert_eq!(released.len(), 2);
    for id in released {
        harness
            .service
            .intercept()
            .send(InterceptCommand::release(id));
    }
    harness.pump(|h| h.terminal_count() == 2);

    assert_eq!(harness.body_of(first), "hello");
    assert_eq!(harness.body_of(second), "hello");
}

// === D3 blocking half ===

#[test]
fn a_blocking_fetch_pauses_and_resolves_inline() {
    // The document takes this path, which is the one request a driver most
    // wants to intercept. The observer resolves the pause *from inside the
    // announcement*, which is exactly the re-entrancy an in-process driver has.
    let port = spawn_server();
    let (service, _events) =
        NetService::new(ResourcePolicy::permissive_localhost()).expect("net service");
    let service = Rc::new(service);
    service.intercept().enable(Vec::new(), false);

    let control = service.intercept();
    service.set_observer(Some(Rc::new(move |event| {
        if let NetworkEvent::Paused { id, .. } = event {
            control.send(InterceptCommand::Fulfill {
                id,
                response: Box::new(FulfilledResponse {
                    status: 200,
                    status_text: String::from("OK"),
                    headers: vec![(String::from("content-type"), String::from("text/html"))],
                    body: b"<p>stub</p>".to_vec(),
                }),
            });
        }
    })));

    let outcome = service
        .fetch_blocking(NetRequest::navigation(format!(
            "http://127.0.0.1:{port}/real.html"
        )))
        .expect("fulfilled");

    assert_eq!(outcome.body.as_ref(), b"<p>stub</p>");
    assert_eq!(outcome.head.status, 200);
}

#[test]
fn a_decision_for_another_request_does_not_extend_a_blocking_park() {
    // `recv_deadline`, not `recv_timeout` in a loop: a foreign-id decision must
    // not restart the clock. Here the foreign decision arrives first and the
    // real one right after; both are consumed and the park ends on the second.
    let port = spawn_server();
    let (service, _events) =
        NetService::new(ResourcePolicy::permissive_localhost()).expect("net service");
    let service = Rc::new(service);
    service.intercept().enable(Vec::new(), false);

    let control = service.intercept();
    let stranger = oxidepage_net::RequestId::from_parts(9999, oxidepage_base::id::FIRST_GENERATION);
    service.set_observer(Some(Rc::new(move |event| {
        if let NetworkEvent::Paused { id, .. } = event {
            control.send(InterceptCommand::release(stranger));
            control.send(InterceptCommand::release(id));
        }
    })));

    let started = std::time::Instant::now();
    let outcome = service
        .fetch_blocking(NetRequest::navigation(format!("http://127.0.0.1:{port}/a")))
        .expect("continued");

    assert_eq!(outcome.body.as_ref(), b"hello");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the park should have ended on the matching decision"
    );
}

// === D8: auth ===

#[test]
fn a_basic_challenge_is_answered_under_the_same_id() {
    let port = spawn_server();
    let (service, _events) =
        NetService::new(ResourcePolicy::permissive_localhost()).expect("net service");
    let service = Rc::new(service);
    service
        .intercept()
        .enable(Vec::new(), /* handle_auth */ true);

    let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let control = service.intercept();
    let sink = Rc::clone(&seen);
    service.set_observer(Some(Rc::new(move |event| match event {
        NetworkEvent::Paused { id, .. } => control.send(InterceptCommand::release(id)),
        NetworkEvent::AuthRequired { id, challenge, .. } => {
            sink.borrow_mut().push(challenge.realm.clone());
            control.send(InterceptCommand::Auth {
                id,
                response: AuthResponse::Provide {
                    username: String::from("alice"),
                    password: String::from("secret"),
                },
            });
        }
        _ => {}
    })));

    let outcome = service
        .fetch_blocking(NetRequest::navigation(format!(
            "http://127.0.0.1:{port}/auth"
        )))
        .expect("authorized");

    assert_eq!(outcome.head.status, 200);
    // `alice:secret` base64-encoded.
    assert_eq!(
        String::from_utf8_lossy(&outcome.body),
        "authorized Basic YWxpY2U6c2VjcmV0"
    );
    assert_eq!(seen.borrow().as_slice(), ["wonderland"], "the realm parsed");
}

#[test]
fn a_default_auth_answer_lets_the_challenge_through() {
    let port = spawn_server();
    let (service, _events) =
        NetService::new(ResourcePolicy::permissive_localhost()).expect("net service");
    let service = Rc::new(service);
    service.intercept().enable(Vec::new(), true);

    let control = service.intercept();
    service.set_observer(Some(Rc::new(move |event| match event {
        NetworkEvent::Paused { id, .. } => control.send(InterceptCommand::release(id)),
        NetworkEvent::AuthRequired { id, .. } => control.send(InterceptCommand::Auth {
            id,
            response: AuthResponse::Default,
        }),
        _ => {}
    })));

    let outcome = service
        .fetch_blocking(NetRequest::navigation(format!(
            "http://127.0.0.1:{port}/auth"
        )))
        .expect("the stashed 401");

    // The response the driver declined to answer is delivered unchanged, not
    // turned into an error.
    assert_eq!(outcome.head.status, 401);
}

#[test]
fn no_challenge_is_raised_when_auth_handling_is_off() {
    let port = spawn_server();
    let harness = Harness::new();
    // `handle_auth: false` — Puppeteer sends `handleAuthRequests` explicitly,
    // and a driver that did not ask must see the 401 itself.
    harness.service.intercept().enable(Vec::new(), false);

    let control = harness.service.intercept();
    let outcome = {
        let control = control.clone();
        harness.service.set_observer(Some(Rc::new(move |event| {
            if let NetworkEvent::Paused { id, .. } = event {
                control.send(InterceptCommand::release(id));
            }
        })));
        harness
            .service
            .fetch_blocking(NetRequest::navigation(format!(
                "http://127.0.0.1:{port}/auth"
            )))
            .expect("the 401 itself")
    };

    assert_eq!(outcome.head.status, 401);
}

// === D7: the timeout is a real backstop, on both halves ===

#[test]
fn an_unanswered_async_pause_gives_up_and_reports_once() {
    // The asynchronous half has no `recv_deadline` to lean on — nothing is
    // waiting on a clock — so the release has to be an explicit sweep. Without
    // it a driver that goes quiet while holding its socket open leaves the
    // request with **no terminal event at all**: `in_flight` never returns to
    // zero, the page is never idle, and every later `settle` burns its whole
    // budget.
    let port = spawn_server();
    let harness = Harness::new();
    enable_all(&harness);

    let id = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:{port}/"),
    ));
    assert_eq!(harness.paused_id(), Some(id));

    // Nobody answers. Rather than spend the real 20 s, reach in and expire it:
    // the property under test is that the sweep runs and releases, not the
    // constant.
    harness.service.expire_pauses_for_test();
    harness.pump(|h| h.terminal_count() == 1);

    // Released as `Continue`, so the request completed rather than failing —
    // a driver that merely stalled must not break the page.
    assert_eq!(harness.body_of(id), "hello");
    assert!(
        harness.service.intercept().paused_ids().is_empty(),
        "the shared set must not retain a released pause"
    );
}

#[test]
fn releasing_everything_also_stops_intercepting() {
    // The half of release-on-disconnect that is easy to miss. Draining the
    // paused set without clearing `enabled` leaves the page pausing every
    // *subsequent* request with nobody left to answer.
    let port = spawn_server();
    let harness = Harness::new();
    enable_all(&harness);

    let first = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:{port}/"),
    ));
    assert_eq!(harness.paused_id(), Some(first));

    // The interceptor goes away.
    harness.service.intercept().release_all();
    harness.pump(|h| h.terminal_count() == 1);

    let second = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/b"),
        format!("http://127.0.0.1:{port}/"),
    ));
    harness.pump(|h| h.terminal_count() == 2);
    assert_eq!(
        harness.body_of(second),
        "hello",
        "a request made after the interceptor left must not pause"
    );
}

// === D8: the retry is bounded, and answers the right header ===

#[test]
fn a_server_that_keeps_refusing_does_not_loop_forever() {
    // `continueWithAuth` re-issues under the same id and goes back through the
    // same pause. A server that refuses the credentials re-challenges, so an
    // unbounded retry is one request per round trip and no terminal event ever.
    // `/always-401` never accepts.
    let port = spawn_server();
    let (service, events) =
        NetService::new(ResourcePolicy::permissive_localhost()).expect("net service");
    let service = Rc::new(service);
    service.intercept().enable(Vec::new(), true);

    let challenges = Rc::new(std::cell::Cell::new(0u32));
    let control = service.intercept();
    let counter = Rc::clone(&challenges);
    service.set_observer(Some(Rc::new(move |event| match event {
        NetworkEvent::Paused { id, .. } => control.send(InterceptCommand::release(id)),
        NetworkEvent::AuthRequired { id, .. } => {
            counter.set(counter.get() + 1);
            control.send(InterceptCommand::Auth {
                id,
                response: AuthResponse::Provide {
                    username: String::from("wrong"),
                    password: String::from("wrong"),
                },
            });
        }
        _ => {}
    })));
    let _ = events;

    let outcome = service
        .fetch_blocking(NetRequest::navigation(format!(
            "http://127.0.0.1:{port}/always-401"
        )))
        .expect("the challenge the driver could not answer");

    assert_eq!(
        outcome.head.status, 401,
        "the second refusal reaches the page"
    );
    assert_eq!(
        challenges.get(),
        1,
        "exactly one retry: a re-challenge must not re-prompt forever"
    );
}

#[test]
fn a_proxy_challenge_is_answered_in_the_proxy_header() {
    // A 407 answered into `Authorization` instead of `Proxy-Authorization` is
    // refused by every proxy there is — and the request then re-challenges
    // forever. `/proxy-auth` accepts only the correct header.
    let port = spawn_server();
    let (service, _events) =
        NetService::new(ResourcePolicy::permissive_localhost()).expect("net service");
    let service = Rc::new(service);
    service.intercept().enable(Vec::new(), true);

    let control = service.intercept();
    service.set_observer(Some(Rc::new(move |event| match event {
        NetworkEvent::Paused { id, .. } => control.send(InterceptCommand::release(id)),
        NetworkEvent::AuthRequired { id, challenge, .. } => {
            assert_eq!(
                format!("{:?}", challenge.source),
                "Proxy",
                "a 407 must be reported as a proxy challenge"
            );
            control.send(InterceptCommand::Auth {
                id,
                response: AuthResponse::Provide {
                    username: String::from("alice"),
                    password: String::from("secret"),
                },
            });
        }
        _ => {}
    })));

    let outcome = service
        .fetch_blocking(NetRequest::navigation(format!(
            "http://127.0.0.1:{port}/proxy-auth"
        )))
        .expect("authorized");

    assert_eq!(outcome.head.status, 200);
    assert_eq!(
        String::from_utf8_lossy(&outcome.body),
        "proxy-authorized Basic YWxpY2U6c2VjcmV0"
    );
}

// === D9: network conditions ===

#[test]
fn offline_fails_every_request_without_touching_the_network() {
    let port = spawn_server();
    let harness = Harness::new();
    harness
        .service
        .intercept()
        .set_conditions(/* offline */ true, Duration::ZERO);

    let outcome = harness
        .service
        .fetch_blocking(NetRequest::navigation(format!("http://127.0.0.1:{port}/a")));

    let error = outcome.expect_err("offline must fail");
    assert!(
        error.to_string().contains("net::ERR_INTERNET_DISCONNECTED"),
        "got {error}"
    );
    assert_eq!(harness.terminal_count(), 1, "and it still reports once");
}

#[test]
fn an_offline_async_request_still_reports_a_terminal_event() {
    let port = spawn_server();
    let harness = Harness::new();
    harness
        .service
        .intercept()
        .set_conditions(true, Duration::ZERO);

    // The caller has already stored its `pending_*` state under this id; a
    // silent drop would leave the page waiting on a load that never resolves.
    let _id = harness.service.start_resource(NetRequest::subresource(
        format!("http://127.0.0.1:{port}/a"),
        format!("http://127.0.0.1:{port}/"),
    ));
    harness.pump(|h| h.terminal_count() == 1);
}

#[test]
fn offline_does_not_break_data_urls() {
    // `data:` involves no network — the bytes are in the URL — so Chrome
    // resolves one with the network disabled and so must this. The offline gate
    // needs the same `http`/`https` predicate the pause gate has, and for the
    // same reason: `fetch_inner` answers `data:` above the scheme gate
    // (ADR-0029). Without it, `emulateNetworkConditions { offline: true }` — the
    // emulation a driver reaches for to test an *offline* page — breaks every
    // inline script, module, stylesheet and `fetch` on it.
    let harness = Harness::new();
    harness
        .service
        .intercept()
        .set_conditions(/* offline */ true, Duration::ZERO);

    let outcome = harness
        .service
        .fetch_blocking(NetRequest::navigation("data:text/plain,still here"));

    assert_eq!(
        outcome.expect("data: must resolve offline").body.as_ref(),
        b"still here"
    );
}

#[test]
fn latency_delays_a_request_without_eating_its_timeout_budget() {
    let port = spawn_server();
    let harness = Harness::new();
    harness
        .service
        .intercept()
        .set_conditions(false, Duration::from_millis(300));

    let started = std::time::Instant::now();
    let outcome = harness
        .service
        .fetch_blocking(NetRequest::navigation(format!("http://127.0.0.1:{port}/a")));

    assert!(outcome.is_ok(), "latency must not become a timeout");
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "the delay was not applied: {:?}",
        started.elapsed()
    );
}
