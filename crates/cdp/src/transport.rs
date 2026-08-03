//! The listener: accept, route, upgrade, and drive one WebSocket connection.
//!
//! ## Threads and tasks
//!
//! A connection is three things running at once:
//!
//! * a **read task** (tokio) pulling frames off the socket and handing each one
//!   to a lane — it must never block, or the connection stops answering
//!   `Browser.close` while a page loads;
//! * a **writer task** (tokio) draining the outbound queue onto the socket;
//! * an **event thread** (OS) reading the registry's broadcast bus, which is a
//!   `crossbeam` receiver and therefore blocking by nature.
//!
//! The lanes themselves belong to [`Connection`](crate::session::Connection).
//! Everything the three share travels through channels, so no page state and no
//! `!Send` value ever reaches this module.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;

use crate::error::ServeError;
use crate::http::Endpoint;
use crate::message::{Outbound, parse_request};
use crate::session::Connection;
use crate::target::TargetRegistry;

/// How long the accept loop lets open connections flush after it stops.
///
/// `Browser.close` is answered on a lane and then queued to that connection's
/// writer, so the reply is in flight at the exact moment the server is told to
/// stop. Puppeteer waits for that reply before disposing its transport, and
/// tearing the runtime down underneath it turns every clean shutdown into a
/// protocol error.
const SHUTDOWN_DRAIN: std::time::Duration = std::time::Duration::from_millis(250);

/// Most sockets served at once.
///
/// A driver opens one. The cap exists because a peer that connects and stays
/// silent holds a task and a descriptor until [`crate::http::HEAD_TIMEOUT`],
/// and enough of them exhaust the process's descriptors — after which
/// `accept()` fails immediately and forever on a listener that stays readable.
const MAX_CONNECTIONS: usize = 64;

/// How long the accept loop pauses after a failed `accept()`.
///
/// Without it, `EMFILE` — the descriptor exhaustion above — spins this loop at
/// 100% CPU: the listener stays readable, so `accept()` returns the same error
/// on every iteration with nothing to wait for.
const ACCEPT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

/// The server's stop signal.
///
/// A bare `Notify` is not enough. `notify_waiters` wakes only those already
/// registered and leaves no permit for a waiter that arrives later, so a stop
/// raised while the accept loop is between registrations would be dropped on the
/// floor and the server would run on until the next connection happened to wake
/// it. The flag is the memory the `Notify` does not have; the `Notify` is the
/// wakeup the flag cannot deliver. Both halves are needed, and so is the order
/// they are used in — the loop registers before it reads the flag, or the same
/// wakeup is lost in the gap between the two (see [`Listener::run`]).
pub struct Shutdown {
    notify: Notify,
    stopping: std::sync::atomic::AtomicBool,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    #[must_use]
    pub fn new() -> Self {
        Self {
            notify: Notify::new(),
            stopping: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Asks the server to stop. Idempotent.
    pub fn trigger(&self) {
        self.stopping.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }
}

/// A bound, running server.
pub struct Listener {
    listener: TcpListener,
    registry: TargetRegistry,
    endpoint: Arc<Endpoint>,
    shutdown: Arc<Shutdown>,
}

impl Listener {
    /// Binds to loopback on `port` (0 picks a free one).
    ///
    /// **Loopback only, always.** The endpoint is unauthenticated remote control
    /// of a process that runs attacker-supplied content; there is no bind
    /// address parameter because there is no correct value other than
    /// `127.0.0.1` (`docs/automation-roadmap.md`, "Security note").
    pub async fn bind(browser: oxidepage_engine::Browser, port: u16) -> Result<Self, ServeError> {
        let addr = format!("127.0.0.1:{port}");
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|source| ServeError::Bind {
                addr: addr.clone(),
                source,
            })?;
        let bound = listener
            .local_addr()
            .map_err(|source| ServeError::Bind { addr, source })?;
        Ok(Self {
            listener,
            registry: TargetRegistry::new(browser),
            endpoint: Arc::new(Endpoint {
                addr: bound,
                token: crate::token::random_hex(),
            }),
            shutdown: Arc::new(Shutdown::new()),
        })
    }

    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.endpoint.addr
    }

    /// The URL to hand a driver.
    #[must_use]
    pub fn browser_ws_url(&self) -> String {
        self.endpoint.browser_ws_url()
    }

    #[must_use]
    pub fn shutdown_signal(&self) -> Arc<Shutdown> {
        Arc::clone(&self.shutdown)
    }

    /// Accepts until `Browser.close` or an external shutdown.
    pub async fn run(self) {
        let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        loop {
            // Registered *before* the flag is read, and that order is the whole
            // point. `notify_waiters` wakes only waiters already in the list and
            // leaves no permit behind, so a `trigger()` landing between a false
            // flag read and the `select!`'s first poll — which is where a
            // `Notified` created inside the `select!` registers — is lost
            // entirely, and the server sleeps in `accept()` until some unrelated
            // connection wakes it. `Browser.close` then never returns.
            let notified = self.shutdown.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.shutdown.is_stopping() {
                break;
            }
            let accepted = tokio::select! {
                () = &mut notified => break,
                accepted = self.listener.accept() => accepted,
            };
            let Ok((stream, _peer)) = accepted else {
                // A failed accept is per-connection (descriptor limits, a peer
                // that vanished between SYN and accept); the listener is still
                // good, so this must not end the server — but it must not spin
                // either, because a descriptor-exhausted listener stays
                // readable and fails instantly every time.
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            };
            // Refusing is better than queueing: a caller past the cap learns
            // now, and the sockets already open keep their descriptors.
            if live.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                drop(stream);
                continue;
            }
            live.fetch_add(1, Ordering::AcqRel);
            let open = Arc::clone(&live);
            let registry = self.registry.clone();
            let endpoint = Arc::clone(&self.endpoint);
            let shutdown = Arc::clone(&self.shutdown);
            tokio::spawn(async move {
                handle(stream, registry, endpoint, shutdown).await;
                open.fetch_sub(1, Ordering::AcqRel);
            });
        }
        // Give the in-flight `Browser.close` reply a chance to reach the wire
        // before the caller drops the runtime out from under its writer task.
        tokio::time::sleep(SHUTDOWN_DRAIN).await;
    }
}

async fn handle(
    mut stream: TcpStream,
    registry: TargetRegistry,
    endpoint: Arc<Endpoint>,
    shutdown: Arc<Shutdown>,
) {
    // Nagle would add up to 40 ms to every small command round trip, and CDP is
    // nothing but small round trips.
    let _ = stream.set_nodelay(true);

    let Some(head) = crate::http::read_head(&mut stream).await else {
        return;
    };

    if !head.upgrade {
        crate::http::serve_http(stream, &head, registry, &endpoint).await;
        return;
    }

    // An upgrade must clear three gates. `Host` first, because a rebinding
    // attempt is not a token problem and answering it with "invalid token"
    // would invite guessing.
    if !crate::http::host_is_local(head.host.as_deref()) {
        reject(stream, "Host header is not a loopback address").await;
        return;
    }
    // Then `Origin`: a browser sends one and a driver does not, and a
    // WebSocket is not subject to CORS — so without this a hostile page
    // reaches the protocol with a perfectly valid loopback `Host`.
    if !crate::http::origin_allowed(head.origin.as_deref()) {
        reject(
            stream,
            "WebSocket upgrades from a web origin are not allowed",
        )
        .await;
        return;
    }
    if !endpoint.authorizes(&head.path) {
        // Refuse before the handshake: an unauthorized peer must not end up
        // holding a WebSocket at all.
        reject(stream, "Invalid DevTools token").await;
        return;
    }

    // The head was consumed to classify the request, so replay it into the
    // handshake — tungstenite expects a stream positioned at the request line.
    let replayed = crate::http::PrefixedStream::new(head.into_raw(), stream);
    let Ok(websocket) = tokio_tungstenite::accept_async(replayed).await else {
        return;
    };
    drive(websocket, registry, shutdown).await;
}

async fn reject(mut stream: TcpStream, reason: &str) {
    use tokio::io::AsyncWriteExt;
    let body = format!(r#"{{"error":"{reason}"}}"#);
    let response = format!(
        "HTTP/1.1 403 Forbidden\r\n\
         Content-Type: application/json; charset=UTF-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

async fn drive(
    websocket: tokio_tungstenite::WebSocketStream<crate::http::PrefixedStream>,
    registry: TargetRegistry,
    shutdown: Arc<Shutdown>,
) {
    let (mut sink, mut source) = websocket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Outbound>();
    let connection = Connection::new(registry.clone(), out_tx, shutdown);

    let writer = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            if sink
                .send(Message::Text(message.to_wire().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // The broadcast bus is a blocking `crossbeam` receiver, so it gets a real
    // thread rather than a task. It ends when the subscription is dropped, which
    // happens when the registry notices the sender is disconnected — i.e. once
    // this thread returns.
    let subscription = registry.subscribe();
    let subscription_id = subscription.id;
    let signals = subscription.signals;
    let event_connection = Arc::clone(&connection);
    let events = std::thread::Builder::new()
        .name(String::from("cdp-events"))
        .spawn(move || {
            while let Ok(signal) = signals.recv() {
                event_connection.handle_signal(signal);
            }
        });

    while let Some(Ok(message)) = source.next().await {
        match message {
            Message::Text(text) => match parse_request(&text) {
                Ok(request) => connection.submit(request),
                // A frame we cannot route still gets an answer, so a driver's
                // pending promise is retired rather than left to time out.
                Err(response) => connection.send(response),
            },
            // tungstenite answers Ping itself and Pong needs no reply.
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => break,
        }
    }

    // Order matters. Unsubscribing first disconnects the event thread's
    // receiver so it returns from `recv()`; only then can the last `Arc` on the
    // connection drop, which drops the outbound sender (ending the writer) and
    // the lane senders (ending each lane once it finishes the command it is on).
    registry.unsubscribe(subscription_id);
    if let Ok(events) = events {
        let _ = events.join();
    }
    drop(connection);
    let _ = writer.await;
}
