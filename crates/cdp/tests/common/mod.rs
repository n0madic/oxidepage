//! A blocking CDP client, for driving the server the way a real driver does.
//!
//! The tests are deliberately *end to end over a socket* rather than direct
//! calls into `dispatch`: the parts most likely to break — the handshake, frame
//! routing, which lane a command lands on, whether an event actually reaches the
//! wire — only exist once there is a socket. Calling the handlers directly would
//! test the easy half.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use oxidepage_cdp::{CdpServer, ServerOptions};
use oxidepage_engine::page_api::ResourcePolicy;
use oxidepage_engine::{
    Browser, BrowserOptions, ContextOptions, DEFAULT_DIALOG_TIMEOUT, DialogPolicy,
};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

/// How long a test waits for one response or event before failing.
///
/// Generous, because a command can legitimately queue behind a page load
/// (ADR-0027 D3), and CI machines are slow. A test that hits it has found a
/// hang, not a slow machine.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// A loopback HTTP server for fixtures.
///
/// CI never touches the internet (design §9), so every navigation test serves
/// its own documents. Raw HTTP/1.1 with `Connection: close`, the same shape
/// `xtask/src/testserver.rs` uses.
pub struct Fixtures {
    port: u16,
    routes: Arc<Vec<(String, String)>>,
}

impl Fixtures {
    /// Starts a server for `routes`, a list of `(path, html)` pairs.
    pub fn start(routes: Vec<(&str, &str)>) -> Self {
        let routes: Arc<Vec<(String, String)>> = Arc::new(
            routes
                .into_iter()
                .map(|(path, body)| (path.to_owned(), body.to_owned()))
                .collect(),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let served = Arc::clone(&routes);
        std::thread::spawn(move || {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fixtures");
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let routes = Arc::clone(&served);
                std::thread::spawn(move || {
                    let mut head = [0u8; 4096];
                    let Ok(read) = stream.read(&mut head) else {
                        return;
                    };
                    let text = String::from_utf8_lossy(&head[..read]);
                    let path = text
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap_or("/")
                        .to_owned();
                    // A `/slow-<ms>` path stalls before answering, so a test can
                    // send a second command *while* a load is genuinely in
                    // flight — the only state in which lane priority is
                    // observable.
                    if let Some(ms) = path
                        .strip_prefix("/slow-")
                        .and_then(|ms| ms.parse::<u64>().ok())
                    {
                        std::thread::sleep(Duration::from_millis(ms));
                    }
                    let (status, body) = match routes.iter().find(|(route, _)| *route == path) {
                        Some((_, body)) => ("200 OK", body.clone()),
                        None => ("404 Not Found", String::from("<title>404</title>")),
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        Self {
            port: rx.recv().expect("fixture server failed to start"),
            routes,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    #[allow(dead_code)]
    pub fn routes(&self) -> &[(String, String)] {
        &self.routes
    }
}

/// A browser plus an endpoint in front of it.
pub struct Harness {
    pub server: CdpServer,
    /// Held so the browser outlives the server: dropping it first would leave
    /// the endpoint serving a browser whose threads have already been joined.
    pub browser: Browser,
}

impl Harness {
    pub fn start() -> Self {
        // Loopback is allowed so fixtures can be served from a local test
        // server; the default policy blocks private hosts, and CI never
        // touches the real internet.
        let browser = Browser::new(BrowserOptions {
            policy: ResourcePolicy::permissive_localhost(),
            // The same context configuration `oxidepage serve` uses. `Ask` is
            // what makes `Page.javascriptDialogOpening` mean anything: under
            // the default `Dismiss` the page answers itself and
            // `Page.handleJavaScriptDialog` has nothing left to answer.
            default_context: ContextOptions {
                dialog_policy: DialogPolicy::Ask {
                    timeout: DEFAULT_DIALOG_TIMEOUT,
                },
                ..ContextOptions::default()
            },
            ..Default::default()
        })
        .expect("browser");
        let server =
            CdpServer::start(browser.clone(), ServerOptions::default()).expect("cdp server");
        Self { server, browser }
    }

    pub fn client(&self) -> Client {
        Client::connect(self.server.browser_ws_url())
    }

    /// A client with one fresh page target attached and `Page` enabled — the
    /// three commands every driver sends before doing anything.
    ///
    /// Returns the target id too, so a test can open a *second* connection onto
    /// the same page.
    pub fn attached(&self) -> (Client, String, String) {
        let mut client = self.client();
        let created = client.call("Target.createTarget", json!({ "url": "about:blank" }));
        let target = created["targetId"].as_str().expect("targetId").to_owned();
        let attached = client.call(
            "Target.attachToTarget",
            json!({ "targetId": &target, "flatten": true }),
        );
        let session = attached["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_owned();
        client.call_on(&session, "Page.enable", json!({}));
        (client, session, target)
    }

    /// A *second* connection attached to an existing target, with `Page`
    /// enabled.
    ///
    /// Sessions belong to a connection. A test that must act while the first
    /// connection is blocked inside a command needs its own session on the same
    /// target — passing the other connection's session id would simply be
    /// refused, and a test that ignores that refusal passes vacuously.
    pub fn attach_existing(&self, target_id: &str) -> (Client, String) {
        let mut client = self.client();
        let attached = client.call(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        );
        let session = attached["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_owned();
        client.call_on(&session, "Page.enable", json!({}));
        (client, session)
    }

    /// A blocking `GET` against the discovery endpoint, returning the body.
    pub fn http_get(&self, path: &str) -> String {
        self.http_request("GET", path)
    }

    /// A well-formed request with a caller-chosen method.
    pub fn http_request(&self, method: &str, path: &str) -> String {
        let mut stream =
            std::net::TcpStream::connect(self.server.addr()).expect("connect to endpoint");
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.server.addr()
        );
        stream.write_all(request.as_bytes()).expect("write request");
        let mut response = String::new();
        // `Connection: close` means read-to-end terminates the body for us.
        let _ = stream.read_to_string(&mut response);
        response
    }

    /// Sends a raw request in `chunks`, pausing between them, and returns the
    /// whole response.
    ///
    /// The pause is the point: it forces the request across separate TCP
    /// segments, which is the case a `peek`-based router silently dropped.
    pub fn http_raw_chunked(&self, chunks: &[&str]) -> String {
        let mut stream =
            std::net::TcpStream::connect(self.server.addr()).expect("connect to endpoint");
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        for (index, chunk) in chunks.iter().enumerate() {
            stream.write_all(chunk.as_bytes()).expect("write chunk");
            stream.flush().expect("flush chunk");
            if index + 1 < chunks.len() {
                std::thread::sleep(Duration::from_millis(80));
            }
        }
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    /// A raw request with a caller-chosen `Host`.
    pub fn http_get_with_host(&self, path: &str, host: &str) -> String {
        self.http_raw_chunked(&[&format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )])
    }

    /// The body half of an HTTP response, with the status line checked.
    pub fn http_body(&self, path: &str, expect_status: &str) -> Value {
        self.http_body_of("GET", path, expect_status)
    }

    /// [`Harness::http_body`] for a method other than `GET`.
    pub fn http_body_of(&self, method: &str, path: &str, expect_status: &str) -> Value {
        let response = self.http_request(method, path);
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expect_status}")),
            "expected {expect_status} for {path}, got:\n{response}"
        );
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or_default();
        serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("body for {path} is not JSON ({e}): {body}"))
    }
}

/// A synchronous protocol client: send a command, block for its answer.
pub struct Client {
    runtime: tokio::runtime::Runtime,
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: i64,
    /// Events that arrived while waiting for a response. Protocol events and
    /// responses share one stream, so anything not being waited for has to be
    /// kept rather than discarded — a test asserting on `targetCreated` would
    /// otherwise lose it to whichever command happened to be in flight.
    events: Vec<Value>,
    /// Answers that arrived while waiting for a *different* command's answer.
    ///
    /// Kept for the same reason `events` are, and for a sharper one: a command
    /// on the priority lane and a [`Client::dispatch`]ed one on its session
    /// lane are answered by different threads (ADR-0032 D4), so the two frames
    /// race on the wire. Discarding the one not being waited for leaves the
    /// later [`Client::collect`] reading until the timeout — a hang that
    /// reproduces only where the scheduler happens to order the two writes the
    /// other way, which is how it stayed invisible until Windows CI.
    responses: Vec<Value>,
}

impl Client {
    pub fn connect(url: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client runtime");
        let (socket, _) = runtime
            .block_on(tokio_tungstenite::connect_async(url))
            .expect("websocket handshake");
        Self {
            runtime,
            socket,
            next_id: 0,
            events: Vec::new(),
            responses: Vec::new(),
        }
    }

    /// Sends a command and blocks for its response, asserting success.
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        match self.try_call(method, params) {
            Ok(result) => result,
            Err(error) => panic!("{method} failed: {error}"),
        }
    }

    /// Like [`Client::call`], but hands back the protocol error instead of
    /// panicking — the shape a P6 assertion needs.
    pub fn try_call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.send(method, params, None);
        self.await_response(id)
    }

    /// A command addressed to a session.
    pub fn call_on(&mut self, session: &str, method: &str, params: Value) -> Value {
        let id = self.send(method, params, Some(session));
        match self.await_response(id) {
            Ok(result) => result,
            Err(error) => panic!("{method} on {session} failed: {error}"),
        }
    }

    pub fn try_call_on(
        &mut self,
        session: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, Value> {
        let id = self.send(method, params, Some(session));
        self.await_response(id)
    }

    /// Sends a command **without** waiting for its answer, returning its id.
    ///
    /// For tests that need two commands in flight on one connection at once —
    /// the only way to observe whether the second queues behind the first.
    pub fn dispatch(&mut self, session: &str, method: &str, params: Value) -> i64 {
        self.send(method, params, Some(session))
    }

    /// Blocks for the answer to a previously [`Client::dispatch`]ed command.
    pub fn collect(&mut self, id: i64) -> Result<Value, Value> {
        self.await_response(id)
    }

    fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.send_raw(&message.to_string());
        id
    }

    /// Writes a frame verbatim — for the malformed-input tests.
    pub fn send_raw(&mut self, frame: &str) {
        let frame = frame.to_owned();
        self.runtime
            .block_on(self.socket.send(Message::Text(frame.into())))
            .expect("send frame");
    }

    /// Reads until the frame carrying `id` arrives, whatever it says.
    ///
    /// For frames written with [`Client::send_raw`], where the test picked the
    /// id itself and wants to see the raw envelope rather than a `Result`.
    pub fn await_frame(&mut self, id: i64) -> Value {
        if let Some(index) = self
            .responses
            .iter()
            .position(|message| message.get("id").and_then(Value::as_i64) == Some(id))
        {
            return self.responses.remove(index);
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let message = self.read_frame(deadline);
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message;
            }
            self.stash(message);
        }
    }

    fn await_response(&mut self, id: i64) -> Result<Value, Value> {
        let message = self.await_frame(id);
        match message.get("error") {
            Some(error) => Err(error.clone()),
            None => Ok(message.get("result").cloned().unwrap_or(json!({}))),
        }
    }

    /// Keeps a frame this call is not waiting for, so a later one can have it.
    fn stash(&mut self, message: Value) {
        if message.get("method").is_some() {
            self.events.push(message);
        } else if message.get("id").is_some() {
            self.responses.push(message);
        }
        // Anything else carries neither: a non-text frame read as `{}`.
    }

    /// The next event named `method`, from the buffer or the wire.
    pub fn await_event(&mut self, method: &str) -> Value {
        self.try_await_event(method)
            .unwrap_or_else(|| panic!("timed out waiting for {method}"))
    }

    pub fn try_await_event(&mut self, method: &str) -> Option<Value> {
        if let Some(index) = self
            .events
            .iter()
            .position(|e| e.get("method").and_then(Value::as_str) == Some(method))
        {
            return Some(self.events.remove(index));
        }
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let message = self.read_frame(deadline);
            if message.get("method").and_then(Value::as_str) == Some(method) {
                return Some(message);
            }
            self.stash(message);
        }
        None
    }

    /// Reads for `settle`, then throws away everything buffered.
    ///
    /// For a test that asserts on the *next* event of a kind: a target's opening
    /// `about:blank` navigation can land either side of the command that enables
    /// the domain, so a test counting events from the start of the stream is
    /// deciding a race. This gives it a known-empty baseline instead.
    pub fn forget_events(&mut self, settle: Duration) {
        let _ = self.drain_events(settle);
        self.events.clear();
    }

    /// Whether an event has arrived *already* — for asserting an event does
    /// **not** fire, without paying the full timeout.
    pub fn drain_events(&mut self, settle: Duration) -> Vec<Value> {
        let deadline = Instant::now() + settle;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let read = self
                .runtime
                .block_on(async { tokio::time::timeout(remaining, self.socket.next()).await });
            match read {
                Ok(Some(Ok(Message::Text(text)))) => {
                    // Responses are stashed, not dropped, even here: this
                    // window is about clearing the *event* baseline, and an
                    // answer thrown away is a `collect` that never returns.
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        self.stash(value);
                    }
                }
                Ok(Some(Ok(_))) => {}
                // Closed, errored, or the settle window elapsed.
                _ => break,
            }
        }
        std::mem::take(&mut self.events)
    }

    fn read_frame(&mut self, deadline: Instant) -> Value {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for a frame");
        let read = self
            .runtime
            .block_on(async { tokio::time::timeout(remaining, self.socket.next()).await });
        match read {
            Ok(Some(Ok(Message::Text(text)))) => {
                serde_json::from_str(&text).expect("server sent invalid JSON")
            }
            Ok(Some(Ok(Message::Close(_))) | None) => panic!("connection closed unexpectedly"),
            Ok(Some(Ok(_))) => json!({}),
            Ok(Some(Err(e))) => panic!("websocket error: {e}"),
            Err(_) => panic!("timed out waiting for a frame"),
        }
    }
}

/// Creates an isolated world and returns its `Runtime.ExecutionContextId`.
///
/// The id comes from the command's own answer rather than from the
/// `Runtime.executionContextCreated` event, so a test can use it without
/// enabling the `Runtime` domain first.
pub fn isolated_world(client: &mut Client, session: &str, name: &str) -> i64 {
    let created = client.call_on(
        session,
        "Page.createIsolatedWorld",
        serde_json::json!({ "worldName": name }),
    );
    created["executionContextId"]
        .as_i64()
        .unwrap_or_else(|| panic!("createIsolatedWorld returned no id: {created}"))
}
