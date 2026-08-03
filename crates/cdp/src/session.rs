//! One connection's view of the browser: its sessions, its command lanes, and
//! its translation of [`TargetSignal`]s into protocol events.
//!
//! ## Why commands run on threads and not on the tokio runtime
//!
//! [`PageHandle::with`] is *blocking*: it queues a closure onto the page thread
//! and parks until the reply comes back. Worse, an ordinary job is deferred
//! while the page is navigating or parsing (ADR-0027 D3), so a command sent
//! during a `goto` can legitimately take as long as the load does. Running that
//! on a tokio worker would stall unrelated I/O; running it on the socket's read
//! loop would stop the connection from accepting `Browser.close` while a page
//! spins.
//!
//! So each session gets a **lane**: one OS thread, one command at a time. That
//! preserves per-session ordering — which drivers rely on, e.g. `Page.enable`
//! must take effect before the `Page.navigate` that follows it — while letting
//! the read loop stay responsive and letting two targets make progress at once.
//! Browser-level commands share a lane of their own.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use oxidepage_engine::PageHandle;
use tokio::sync::mpsc;

use crate::transport::Shutdown;

use crate::error::{CommandResult, ProtocolError};
use crate::message::{Event, Outbound, Request, Response};
use crate::target::{TargetInfo, TargetRegistry, TargetSignal};
use crate::token::random_hex;

/// The lane browser-level (session-less) commands run on. Not a valid session
/// id — those are 32 hex characters — so it cannot collide with one.
const BROWSER_LANE: &str = "";

/// Most sessions one connection may hold.
///
/// Each spawns a lane thread on its first command, so an uncapped
/// `Target.attachToTarget` loop is the same thread exhaustion the
/// unknown-session guard closes for *invented* ids. A driver holds one session
/// per target and a handful of targets.
const MAX_SESSIONS: usize = 256;

/// The lane for commands that must not queue behind the session's own work.
///
/// A session lane is serial by design, and that is right for ordinary commands.
/// It is fatal for a dialog answer: the page parks inside `alert()` *during*
/// `Page.navigate`, so the navigate still occupies the lane, and the answer
/// that would release it sits behind the very command it must unblock. The
/// dialog then auto-dismisses on timeout and the answer arrives to find nothing
/// showing — the exact shape `page.on('dialog', d => d.accept())` takes during
/// `page.goto()`.
///
/// Not the read loop either: that must stay free to accept `Browser.close`.
const PRIORITY_LANE: &str = "\u{1}priority";

/// Methods routed to [`PRIORITY_LANE`].
///
/// Kept deliberately short, and the bar is **not** "important": one lane is
/// shared by every session on the connection, so a method that can block holds
/// up every *other* target's urgent command too. A method belongs here only if
/// it is meant to interrupt work in flight **and** cannot block itself.
///
/// `Page.stopLoading` reads as a fit and is not one: the page thread is inside a
/// blocking document fetch for the whole of a slow load and services nothing,
/// so this would sit on the shared lane for seconds and strand the dialog answer
/// of an unrelated target. It runs on its own session's lane, where the only
/// thing it delays is the session that sent it — and it is a *control* call
/// (`PageHandle::stop_loading`), so it answers at the first wait point rather
/// than after the whole navigation.
///
/// `Browser.close` joins every page thread and so can block too. It stays,
/// because nothing queued behind it has anywhere to go: the browser and this
/// server are both ending.
/// `Fetch`'s resolution commands belong here for the dialog's exact reason
/// (ADR-0032 D4): a `Page.navigate` occupies its session's lane for the whole
/// load, and the document fetch it is blocked on is precisely the request a
/// driver pauses — so a `continueRequest` queued behind that navigation would
/// deadlock against the command that would release it. `Fetch.disable` joins
/// them because it releases *every* paused request.
///
/// They clear the "cannot block itself" half of the bar because the decision
/// channel is unbounded: each one takes a mutex, removes an id from a set, and
/// sends without waiting for a receiver.
///
/// **`Fetch.enable` is deliberately not here.** It only writes shared config,
/// and the driver's own lane already orders it before the `Page.navigate` that
/// follows — putting it on the shared lane would let it overtake commands of
/// its own session that must precede it.
fn is_priority(method: &str) -> bool {
    matches!(
        method,
        "Page.handleJavaScriptDialog"
            | "Browser.close"
            | "Fetch.continueRequest"
            | "Fetch.fulfillRequest"
            | "Fetch.failRequest"
            | "Fetch.continueWithAuth"
            | "Fetch.disable"
    )
}

/// Which domains a session has enabled.
///
/// CDP domains are opt-in per session: a driver that never sends `Page.enable`
/// must not receive `Page.lifecycleEvent`. Keeping the flags here rather than on
/// the page is what lets two sessions watch one target with different interests.
#[derive(Debug, Default)]
pub struct DomainFlags {
    pub page: AtomicBool,
    pub runtime: AtomicBool,
    pub log: AtomicBool,
    pub network: AtomicBool,
    /// `Page.setLifecycleEventsEnabled` — separate from `Page.enable`, because
    /// Puppeteer's `waitUntil: 'networkidle0'` turns it on independently.
    pub lifecycle: AtomicBool,
    /// `DOM.enable`. Its one consequence is `DOM.documentUpdated` on a commit
    /// (ADR-0031 D2) — the honest signal that every node id issued so far is
    /// dead. There is no pushed node tree, so nothing else hangs off it.
    pub dom: AtomicBool,
    /// `Fetch.enable`. Gates `Fetch.requestPaused`/`authRequired` (ADR-0032).
    ///
    /// The interception *state* is shared browser-side on the page, not here:
    /// two sessions both intercepting resolve against one paused set, and the
    /// first decision wins. This flag only decides who is told.
    pub fetch: AtomicBool,
}

/// One attachment of a connection to a target.
pub struct SessionState {
    pub id: String,
    pub target_id: String,
    pub page: PageHandle,
    pub flags: DomainFlags,
    /// World names this session asked `Page.createIsolatedWorld` for.
    ///
    /// Remembered because a new document clears every execution context, and a
    /// driver's utility realm re-binds by *name*: without re-announcing them
    /// after each commit, every isolated-realm call — `page.title`, `page.$`,
    /// `waitForSelector` — blocks until the driver's own timeout.
    isolated_worlds: Mutex<Vec<String>>,
    /// Binding names this session installed with `Runtime.addBinding`.
    ///
    /// `Runtime.bindingCalled` goes only to the sessions that asked for that
    /// name. Broadcasting to every session on the target instead fires a
    /// driver's callback once per attached session for a single page call, and
    /// delivers the event to a session that never installed the binding.
    bindings: Mutex<Vec<String>>,
}

impl SessionState {
    /// The position of `name` in this session's world list, appending it if it
    /// is new. That position is what gives each world a context id of its own.
    #[must_use]
    pub fn isolated_world_index(&self, name: &str) -> usize {
        let mut worlds = self
            .isolated_worlds
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match worlds.iter().position(|existing| existing == name) {
            Some(index) => index,
            None => {
                worlds.push(name.to_owned());
                worlds.len() - 1
            }
        }
    }

    /// Records a binding name this session installed.
    pub fn remember_binding(&self, name: &str) {
        let mut bindings = self.bindings.lock().unwrap_or_else(|e| e.into_inner());
        if !bindings.iter().any(|existing| existing == name) {
            bindings.push(name.to_owned());
        }
    }

    #[must_use]
    pub fn wants_binding(&self, name: &str) -> bool {
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|existing| existing == name)
    }

    #[must_use]
    pub fn isolated_worlds(&self) -> Vec<String> {
        self.isolated_worlds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// A connection's mutable state, shared between its read loop, its lanes and
/// its event thread.
pub struct Connection {
    pub registry: TargetRegistry,
    /// The writer half. A tokio channel rather than a `crossbeam` one because
    /// both ends matter: lanes and the event thread are plain OS threads and
    /// need a *non-async* `send`, while the socket writer is a tokio task and
    /// needs an `await`able `recv`. `UnboundedSender::send` is synchronous, so
    /// this is the one channel type that serves both without a bridge thread.
    out: mpsc::UnboundedSender<Outbound>,
    /// Signalled by `Browser.close`, awaited by the accept loop.
    pub shutdown: Arc<Shutdown>,
    /// Set by `Browser.close`, acted on by the lane once the reply is queued.
    shutdown_armed: AtomicBool,
    sessions: Mutex<HashMap<String, Arc<SessionState>>>,
    lanes: Mutex<HashMap<String, Sender<Lane>>>,
    /// `Target.setDiscoverTargets`.
    pub discover: AtomicBool,
    /// `Target.setAutoAttach`.
    pub auto_attach: AtomicBool,
    /// `Target.setAutoAttach { waitForDebuggerOnStart }` — pages are created
    /// suspended and wait for `Runtime.runIfWaitingForDebugger`.
    pub wait_for_debugger: AtomicBool,
    /// Buffers handed out as `IO` stream handles, with each one's read
    /// position. Per connection, because a handle is only meaningful to the
    /// socket it was minted on.
    streams: Mutex<HashMap<String, Stream>>,
}

/// Lets every request `session` holds paused proceed, unmodified.
fn release_interception(session: &Arc<SessionState>) {
    session.flags.fetch.store(false, Ordering::Relaxed);
    session.page.release_paused_requests();
}

type Lane = Box<dyn FnOnce() + Send + 'static>;

/// A buffer handed out as an `IO` stream handle, with its read position.
///
/// `Arc<Vec<u8>>` so a read can clone the handle out of the map and release the
/// lock before base64-encoding a megabyte of it.
type Stream = (Arc<Vec<u8>>, usize);

impl Connection {
    #[must_use]
    pub fn new(
        registry: TargetRegistry,
        out: mpsc::UnboundedSender<Outbound>,
        shutdown: Arc<Shutdown>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            out,
            shutdown,
            shutdown_armed: AtomicBool::new(false),
            sessions: Mutex::new(HashMap::new()),
            lanes: Mutex::new(HashMap::new()),
            discover: AtomicBool::new(false),
            auto_attach: AtomicBool::new(false),
            wait_for_debugger: AtomicBool::new(false),
            streams: Mutex::new(HashMap::new()),
        })
    }

    /// Queues one frame's work onto the lane that owns it.
    ///
    /// Called from the socket read loop, so it must never block on the page.
    pub fn submit(self: &Arc<Self>, request: Request) {
        let session_lane = request.session_id.clone().unwrap_or_default();
        // Kept for the failure paths below, which run after `request` has moved
        // into the job.
        let request_id = request.id;
        let session_id = request.session_id.clone();

        // Refuse an unknown session *here*, before a lane exists for it. A lane
        // is an OS thread that lives until the connection closes, and only
        // `detach` ever removes one — so creating a lane first and validating
        // inside it would let a client spawn a thread per made-up `sessionId`
        // and exhaust the process's threads. `session_id` comes straight off an
        // untrusted frame, so this is reachable by anyone who can connect.
        if session_lane != BROWSER_LANE && self.session(&session_lane).is_none() {
            self.send(Response::err(
                request_id,
                session_id,
                ProtocolError::no_session(&session_lane),
            ));
            return;
        }
        let lane_key = if is_priority(&request.method) {
            String::from(PRIORITY_LANE)
        } else {
            session_lane
        };

        let connection = Arc::clone(self);
        let job: Lane = Box::new(move || {
            let session_id = request.session_id.clone();
            let result = connection.dispatch(&request);
            connection.send(Response::from_result(request.id, session_id, result));
            // Fired only after the reply is queued to the writer — see
            // `domains::browser::close` for why the order is load-bearing.
            if connection.shutdown_armed.load(Ordering::Acquire) {
                connection.shutdown.trigger();
            }
        });

        let mut lanes = self.lock_lanes();
        let sender = lanes.entry(lane_key.clone()).or_insert_with(|| {
            let (tx, rx) = crossbeam_channel::unbounded::<Lane>();
            // Truncated by *characters*, not bytes: `lane_key` is client-chosen
            // and a byte slice landing mid-codepoint is a panic in the read
            // loop, which takes the whole connection down.
            let name = if lane_key == BROWSER_LANE {
                String::from("cdp-lane-browser")
            } else if lane_key == PRIORITY_LANE {
                String::from("cdp-lane-priority")
            } else {
                let short: String = lane_key.chars().take(8).collect();
                format!("cdp-lane-{short}")
            };
            // A lane that cannot be spawned would silently swallow every command
            // on it, so fall back to running inline on the read loop: slower,
            // but the connection still answers.
            if std::thread::Builder::new()
                .name(name)
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        job();
                    }
                })
                .is_err()
            {
                let (dead_tx, _) = crossbeam_channel::unbounded::<Lane>();
                return dead_tx;
            }
            tx
        });

        if sender.send(job).is_err() {
            // The lane thread is gone (or was never spawned). Answer rather than
            // leaving the client's promise pending forever.
            self.send(Response::err(
                request_id,
                session_id,
                ProtocolError::internal("command lane unavailable"),
            ));
        }
    }

    fn lock_lanes(&self) -> std::sync::MutexGuard<'_, HashMap<String, Sender<Lane>>> {
        self.lanes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<SessionState>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Retains `bytes` as an `IO` stream, returning its handle.
    pub fn open_stream(&self, bytes: Vec<u8>) -> String {
        let handle = random_hex();
        self.lock_streams()
            .insert(handle.clone(), (Arc::new(bytes), 0));
        handle
    }

    #[must_use]
    pub fn stream_bytes(&self, handle: &str) -> Option<Arc<Vec<u8>>> {
        self.lock_streams()
            .get(handle)
            .map(|(bytes, _)| Arc::clone(bytes))
    }

    #[must_use]
    pub fn stream_position(&self, handle: &str) -> usize {
        self.lock_streams()
            .get(handle)
            .map_or(0, |(_, position)| *position)
    }

    pub fn set_stream_position(&self, handle: &str, position: usize) {
        if let Some(entry) = self.lock_streams().get_mut(handle) {
            entry.1 = position;
        }
    }

    pub fn close_stream(&self, handle: &str) {
        self.lock_streams().remove(handle);
    }

    fn lock_streams(&self) -> std::sync::MutexGuard<'_, HashMap<String, Stream>> {
        self.streams.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Marks the server as stopping *after* the current reply is written.
    pub fn arm_shutdown(&self) {
        self.shutdown_armed.store(true, Ordering::Release);
    }

    /// Writes one message to the socket. Silently drops if the writer is gone —
    /// the connection is closing, and there is no one left to tell.
    pub fn send(&self, message: impl Into<Outbound>) {
        let _ = self.out.send(message.into());
    }

    pub fn emit(&self, event: Event) {
        self.send(Outbound::Event(event));
    }

    /// Routes one command to its domain.
    ///
    /// The `MethodNotFound` default is the P6 contract: a domain we have not
    /// implemented is *absent*, not stubbed, so `page.setGeolocation()` fails
    /// loudly instead of appearing to work.
    fn dispatch(self: &Arc<Self>, request: &Request) -> CommandResult {
        // A `sessionId` naming a session this connection never opened is an
        // error even for a browser-level domain: it means the driver's view of
        // its own attachments has diverged, and answering anyway hides that.
        if let Some(session_id) = &request.session_id
            && self.session(session_id).is_none()
        {
            return Err(ProtocolError::no_session(session_id));
        }

        match request.domain() {
            "Browser" => crate::domains::browser::dispatch(self, request),
            "DOM" => crate::domains::dom::dispatch(self, request),
            "Emulation" => crate::domains::emulation::dispatch(self, request),
            "Fetch" => crate::domains::fetch::dispatch(self, request),
            "Input" => crate::domains::input::dispatch(self, request),
            "IO" => crate::domains::io::dispatch(self, request),
            "Log" => crate::domains::log::dispatch(self, request),
            "Network" => crate::domains::network::dispatch(self, request),
            "Page" => crate::domains::page::dispatch(self, request),
            "Performance" => crate::domains::performance::dispatch(self, request),
            "Runtime" => crate::domains::runtime::dispatch(self, request),
            "Security" => crate::domains::emulation::dispatch_security(self, request),
            "Target" => crate::domains::target::dispatch(self, request),
            _ => Err(ProtocolError::method_not_found(&request.method)),
        }
    }

    #[must_use]
    pub fn session(&self, session_id: &str) -> Option<Arc<SessionState>> {
        self.lock_sessions().get(session_id).cloned()
    }

    /// The session a command must have supplied, or `InvalidParams`.
    pub fn require_session(&self, request: &Request) -> Result<Arc<SessionState>, ProtocolError> {
        let Some(session_id) = &request.session_id else {
            return Err(ProtocolError::invalid_params(format!(
                "{} requires a sessionId",
                request.method
            )));
        };
        self.session(session_id)
            .ok_or_else(|| ProtocolError::no_session(session_id))
    }

    /// Attaches to `target_id`, returning the new session id.
    ///
    /// Attaching twice to one target from one connection returns a *second*
    /// session, exactly as Chrome does — a driver may deliberately keep two
    /// sessions with different domains enabled.
    pub fn attach(&self, target_id: &str) -> Result<String, ProtocolError> {
        let page = self
            .registry
            .page(target_id)
            .ok_or_else(|| ProtocolError::no_target(target_id))?;
        if self.lock_sessions().len() >= MAX_SESSIONS {
            return Err(ProtocolError::server(format!(
                "Too many sessions on this connection (limit {MAX_SESSIONS})"
            )));
        }
        let session_id = random_hex();
        let state = Arc::new(SessionState {
            id: session_id.clone(),
            target_id: target_id.to_owned(),
            page,
            flags: DomainFlags::default(),
            isolated_worlds: Mutex::new(Vec::new()),
            bindings: Mutex::new(Vec::new()),
        });
        self.lock_sessions()
            .insert(session_id.clone(), Arc::clone(&state));
        self.registry.update_info(target_id, None, Some(true));

        if let Some(info) = self.registry.info(target_id) {
            self.emit(Event::browser(
                "Target.attachedToTarget",
                serde_json::json!({
                    "sessionId": session_id,
                    "targetInfo": info,
                    "waitingForDebugger": self.wait_for_debugger.load(Ordering::Relaxed),
                }),
            ));
        }
        Ok(session_id)
    }

    /// Detaches one session and announces it.
    pub fn detach(&self, session_id: &str) -> Option<Arc<SessionState>> {
        let state = self.lock_sessions().remove(session_id)?;
        // The interceptor is going away, so nothing will ever answer the
        // requests it holds. Released unmodified, not failed — what Chrome
        // does, and the safe answer for a driver that merely detached
        // (ADR-0032 D7).
        if state.flags.fetch.load(Ordering::Relaxed) {
            release_interception(&state);
        }
        // The lane is retired with the session: its thread exits once the
        // sender drops, after finishing whatever command is in flight.
        self.lock_lanes().remove(session_id);
        if !self
            .lock_sessions()
            .values()
            .any(|s| s.target_id == state.target_id)
        {
            self.registry
                .update_info(&state.target_id, None, Some(false));
        }
        self.emit(Event::browser(
            "Target.detachedFromTarget",
            serde_json::json!({
                "sessionId": session_id,
                "targetId": state.target_id,
            }),
        ));
        Some(state)
    }

    /// Releases every request the connection's sessions hold paused.
    ///
    /// The socket closing is one of the four explicit release paths (ADR-0032
    /// D7): the page owns a sender, so the decision channel never disconnects
    /// and there is no automatic signal to hang this off.
    pub fn release_all_interception(&self) {
        for session in self.lock_sessions().values() {
            if session.flags.fetch.load(Ordering::Relaxed) {
                release_interception(session);
            }
        }
    }

    #[must_use]
    pub fn sessions_for(&self, target_id: &str) -> Vec<Arc<SessionState>> {
        self.lock_sessions()
            .values()
            .filter(|s| s.target_id == target_id)
            .cloned()
            .collect()
    }

    /// Turns one broadcast signal into whatever this connection should hear.
    pub fn handle_signal(self: &Arc<Self>, signal: TargetSignal) {
        match signal {
            TargetSignal::Created(info) => self.on_target_created(info),
            TargetSignal::InfoChanged(info) => {
                if self.discover.load(Ordering::Relaxed) {
                    self.emit(Event::browser(
                        "Target.targetInfoChanged",
                        serde_json::json!({ "targetInfo": info }),
                    ));
                }
            }
            TargetSignal::Destroyed { target_id } => self.on_target_destroyed(&target_id),
            TargetSignal::Page { target_id, event } => {
                crate::pump::dispatch_page_event(self, &target_id, &event);
            }
        }
    }

    fn on_target_created(self: &Arc<Self>, info: TargetInfo) {
        if self.discover.load(Ordering::Relaxed) {
            self.emit(Event::browser(
                "Target.targetCreated",
                serde_json::json!({ "targetInfo": info }),
            ));
        }
        // Auto-attach is what Puppeteer and Playwright both use in place of
        // discovering and then attaching by hand; a target created while it is
        // on must arrive already attached.
        if self.auto_attach.load(Ordering::Relaxed) {
            let _ = self.attach(&info.target_id);
        }
    }

    fn on_target_destroyed(self: &Arc<Self>, target_id: &str) {
        for session in self.sessions_for(target_id) {
            self.detach(&session.id);
        }
        if self.discover.load(Ordering::Relaxed) {
            self.emit(Event::browser(
                "Target.targetDestroyed",
                serde_json::json!({ "targetId": target_id }),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Request;
    use oxidepage_engine::{Browser, BrowserOptions};

    fn connection() -> (Arc<Connection>, mpsc::UnboundedReceiver<Outbound>) {
        let browser = Browser::new(BrowserOptions::default()).expect("browser");
        let registry = TargetRegistry::new(browser);
        let (tx, rx) = mpsc::unbounded_channel();
        (Connection::new(registry, tx, Arc::new(Shutdown::new())), rx)
    }

    fn request(id: i64, method: &str, session: Option<&str>) -> Request {
        let mut value = serde_json::json!({ "id": id, "method": method });
        if let Some(session) = session {
            value["sessionId"] = serde_json::json!(session);
        }
        serde_json::from_value(value).expect("request")
    }

    #[test]
    fn an_unknown_session_spawns_no_lane() {
        let (connection, mut out) = connection();

        // A lane is an OS thread that outlives the command and is only removed
        // by `detach`. Creating one before validating the session let any
        // client spawn a thread per invented `sessionId`.
        for index in 0..64 {
            connection.submit(request(
                index,
                "Target.getTargets",
                Some(&format!("{index:032x}")),
            ));
        }
        assert_eq!(
            connection.lock_lanes().len(),
            0,
            "an unknown session must not leave a lane behind"
        );

        // Each one was still answered, so the client is not left waiting.
        for _ in 0..64 {
            let Some(Outbound::Response(response)) = out.blocking_recv() else {
                panic!("expected a response");
            };
            assert_eq!(
                response.error.expect("unknown session must fail").code,
                ProtocolError::SERVER_ERROR
            );
        }
    }

    #[test]
    fn a_browser_level_command_gets_exactly_one_lane() {
        let (connection, _out) = connection();
        connection.submit(request(1, "Browser.getVersion", None));
        connection.submit(request(2, "Browser.getVersion", None));
        assert_eq!(
            connection.lock_lanes().len(),
            1,
            "session-less commands share one lane"
        );
    }
}
