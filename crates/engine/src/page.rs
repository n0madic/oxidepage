//! [`PageHandle`]: a `Send + Sync` façade over a page living on its own thread.
//!
//! A `Page` is permanently `!Send`, so every method here is the same shape: put
//! a closure on the command channel, block on a typed reply. `call` is the one
//! helper that does it; the typed methods below are each one line over it, and
//! [`PageHandle::with`] covers the whole tail of the `Page` API without thirty
//! more wrappers (ADR-0027 D2).

use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender};
use oxidepage_page::{
    BoxQuads, CallArgument, DialogRequest, DialogResponse, EvaluateOptions, EvaluateOutcome,
    FrameId, FrameInfo, InterceptControl, KeyEvent, KeyInput, LayoutMetrics, LoopStats, MouseInput,
    NavigationHistory, NodeDescription, NodeRef, OpenWindowRequest, OpenedWindow, Page, PageJob,
    PageOptions, PageRecord, PaintOptions, PdfOptions, Point, PropertyDescriptor, Rect,
    RemoteError, RemoteObject, ScreenshotOptions, SharedLocalStorage, SharedNetConfig, Viewport,
    WaitUntil, WheelInput, WindowOp,
};

use crate::context::{BrowserContext, PageSettings};
use crate::dialog::DialogPolicy;
use crate::error::{EngineError, EngineResult};
use crate::event::PageEvent;
use crate::options::NewPageOptions;

/// The page thread's half of the dialog rendezvous.
struct DialogChannel {
    answers: Receiver<DialogResponse>,
}

/// What the page thread hands back to its handle once the page exists.
///
/// Both of these are made *by* `Page::new`, on the page thread, and both are
/// needed *by* the handle, on the driver's — so they travel back over a
/// one-slot channel the launcher drains before it returns.
struct PageControls {
    /// The page's own "a dialog is open" flag.
    dialog_flag: Arc<AtomicBool>,
    /// The driver's handle on request interception (ADR-0032 D2).
    intercept: InterceptControl,
}

/// A type-erased unit of work, before it is tagged control or ordinary.
type BoxedWork = Box<dyn FnOnce(&Page) + Send>;

/// What an opener can ask of a page it opened: the sibling's command channel
/// and its event bus, and nothing more (see [`PageHandle::window_ops`]).
pub(crate) struct WindowOps {
    cmd_tx: Sender<PageJob>,
    event_tx: Sender<PageEvent>,
}

impl WindowOps {
    /// Applies one fire-and-forget [`WindowOp`].
    ///
    /// Never blocks: the caller is the *opener's* page thread, with JavaScript
    /// on its stack and its own DOM borrowed.
    pub(crate) fn apply(&self, op: WindowOp) {
        match op {
            WindowOp::Navigate(url) => {
                let _ = self.cmd_tx.send(PageJob::new(move |page| {
                    let _ = page.navigate(&url, WaitUntil::Load);
                }));
            }
            WindowOp::Close => {
                // The control job, without the join `close()` would do — the
                // opener must not park waiting for a sibling to wind down.
                let _ = self.cmd_tx.send(PageJob::control(Page::request_close));
            }
            // No window manager exists here, so this is reported rather than
            // obeyed. Told, not silently dropped (P6).
            WindowOp::Focus => {
                let _ = self.event_tx.try_send(PageEvent::FocusRequested);
            }
        }
    }
}

/// How long a failed round trip waits for the page thread to say *why* it
/// failed. Only ever paid on an error path, and only until the thread finishes
/// unwinding — see [`PageHandle::gone`].
const CRASH_REPORT_GRACE: Duration = Duration::from_secs(2);

/// How long the page thread's epilogue will wait for room on the event bus to
/// deliver its final [`PageEvent::Closed`] / [`PageEvent::Crashed`].
const TERMINAL_EVENT_GRACE: Duration = Duration::from_millis(250);

/// How long [`PageHandle::answer_dialog`] will wait for the page to reach its
/// receive. Covers the gap between the `DialogOpening` event and the page's
/// `recv`, nothing more.
const DIALOG_ANSWER_GRACE: Duration = Duration::from_secs(2);

/// How long a `window.open` waits for the page it is opening to exist.
///
/// Deliberately *not* the driver's command timeout. This wait happens on the
/// **opener's** page thread with JavaScript on the stack, where the
/// `ScriptBudget` interrupt cannot fire (the block is in Rust) and no control
/// job can reach it — so it is a script-blocking budget, and it is sized like
/// one. Past it `window.open` returns `null`, the popup-blocked answer.
pub(crate) const OPEN_WINDOW_TIMEOUT: Duration = Duration::from_secs(5);

/// Identifies a page within a [`Browser`](crate::Browser).
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct PageId(pub u64);

impl std::fmt::Display for PageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "page-{}", self.0)
    }
}

/// Everything the driver side of a page holds.
pub(crate) struct PageInner {
    id: PageId,
    cmd_tx: Sender<PageJob>,
    events: Receiver<PageEvent>,
    /// The bus's sending half, so the driver can report something the page
    /// itself did not produce — a sibling's `w.focus()`, for one.
    event_tx: Sender<PageEvent>,
    /// Answers for a [`DialogPolicy::Ask`] dialog. Deliberately not the
    /// command channel: the page runs no ordinary job while parked in a
    /// dialog, so an answer queued there would never be reached (D11).
    dialog_tx: Sender<DialogResponse>,
    /// Mirrors [`Page::suspend`]/[`Page::resume`], so `waitingForDebugger` can
    /// be answered without a round trip.
    ///
    /// The same shape as `closed`, and for a sharper reason: this is read while
    /// building `Target.attachedToTarget` on a connection's **event thread**,
    /// which must never block. A `with_control` call looks cheap — the closure
    /// body reads one `Cell` — but it is still a queued job and a reply
    /// channel, and a page spinning in script offers no wait point, so the
    /// round trip could stall every event for every target on that connection
    /// for the whole command timeout.
    ///
    /// Authoritative because [`PageHandle`] is the only way the protocol
    /// suspends or resumes a page; an embedder reaching past it with
    /// `PageHandle::with` would desynchronize this, exactly as it would
    /// `closed`.
    suspended: Arc<AtomicBool>,
    /// Set while the page is inside `run_dialog`.
    ///
    /// The answer channel is a rendezvous, so a send only lands while a receive
    /// is actually in progress. This flag is what lets `answer_dialog`
    /// distinguish "no dialog is open" (refuse immediately) from "the page is
    /// about to start waiting" (block briefly, do not drop the answer).
    dialog_pending: Arc<AtomicBool>,
    /// The driver's handle on this page's request interception (ADR-0032 D2).
    ///
    /// `None` only if the page thread died between construction and publishing
    /// its controls, which every other path already treats as a dead page.
    intercept: Option<InterceptControl>,
    /// How this page answers dialogs.
    ///
    /// Only [`DialogPolicy::Ask`] ever reads the answer channel. Without this,
    /// `answer_dialog` on an auto-dismissing page saw the flag up — the page
    /// *is* in `run_dialog` — and blocked on a rendezvous nobody would ever
    /// complete, so every `alert()` cost a driver following the documented
    /// flow a full `DIALOG_ANSWER_GRACE`.
    dialog_policy: DialogPolicy,
    /// "This page is closed" as script and the embedder see it. Set by the
    /// page thread on its way out **and** by a sibling's `w.close()`, so
    /// `w.closed` reads `true` on the line after the call, as in a browser.
    closed: Arc<AtomicBool>,
    /// "The page thread has left `run_page_thread`." Set only by the thread
    /// epilogue.
    ///
    /// Deliberately *not* `closed`: an opener's `w.close()` sets that flag
    /// from another thread while the sibling is still running (it may be parked
    /// in a dialog for 30 s). [`PageHandle::join_bounded`] skips its poll once
    /// the flag it watches is set and then calls `JoinHandle::join`, which is
    /// only guaranteed not to block if the thread really has finished — so it
    /// watches this one.
    exited: Arc<AtomicBool>,
    /// Why the thread stopped, if it stopped badly.
    crash: Arc<Mutex<Option<String>>>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
    command_timeout: Duration,
    close_timeout: Duration,
}

/// A `Send + Sync` handle to a page running on its own thread.
///
/// Cloning is cheap and gives another handle to the *same* page.
#[derive(Clone)]
pub struct PageHandle(pub(crate) Arc<PageInner>);

impl std::fmt::Debug for PageHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageHandle")
            .field("id", &self.0.id)
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl PageHandle {
    #[must_use]
    pub fn id(&self) -> PageId {
        self.0.id
    }

    /// The push event stream (ADR-0027 D6). Cloning the receiver splits the
    /// stream between consumers rather than duplicating it, so keep one reader.
    #[must_use]
    pub fn events(&self) -> Receiver<PageEvent> {
        self.0.events.clone()
    }

    /// Whether this page is closed. Read from an atomic — no round trip, so it
    /// stays truthful for a page that cannot answer one.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    /// Whether the page's thread has actually finished.
    #[must_use]
    pub fn has_exited(&self) -> bool {
        self.0.exited.load(Ordering::Acquire)
    }

    /// Runs `f` **on the page thread** and returns what it produced.
    ///
    /// The escape hatch for everything the typed methods do not cover. A
    /// closure can hold a `Ref<'_, DomTree>` — which could never cross a
    /// channel — for as long as it needs, and send back an owned projection:
    ///
    /// ```no_run
    /// # use oxidepage_engine::{Browser, BrowserOptions, NewPageOptions};
    /// # let browser = Browser::new(BrowserOptions::default()).unwrap();
    /// # let page = browser.default_context().new_page(NewPageOptions::default()).unwrap();
    /// let url = page.with(|p| p.dom().document_url().to_owned())?;
    /// # Ok::<_, oxidepage_engine::EngineError>(())
    /// ```
    pub fn with<T, F>(&self, f: F) -> EngineResult<T>
    where
        F: FnOnce(&Page) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.call(PageJob::new, f)
    }

    /// Like [`PageHandle::with`], but the closure runs at whatever wait point
    /// receives it, *including* one nested inside a navigation.
    ///
    /// Only sound for work that touches `Cell`s and channels — the page may be
    /// holding borrows on its DOM, style and layout. Reaching any of those from
    /// here is a `BorrowMutError` waiting for the right timing.
    fn with_control<T, F>(&self, f: F) -> EngineResult<T>
    where
        F: FnOnce(&Page) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.call(PageJob::control, f)
    }

    fn call<T, F>(&self, wrap: fn(BoxedWork) -> PageJob, f: F) -> EngineResult<T>
    where
        F: FnOnce(&Page) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.call_within(wrap, self.0.command_timeout, f)
    }

    /// [`PageHandle::call`] with an explicit reply deadline, for the one caller
    /// whose work is legitimately allowed to outlast the command timeout.
    fn call_within<T, F>(
        &self,
        wrap: fn(BoxedWork) -> PageJob,
        timeout: Duration,
        f: F,
    ) -> EngineResult<T>
    where
        F: FnOnce(&Page) -> T + Send + 'static,
        T: Send + 'static,
    {
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let job: BoxedWork = Box::new(move |page| {
            let _ = reply_tx.send(f(page));
        });
        self.send(wrap(job))?;
        match reply_rx.recv_timeout(timeout) {
            Ok(value) => Ok(value),
            Err(RecvTimeoutError::Timeout) => Err(EngineError::Timeout),
            // The reply sender was dropped without answering: the thread died
            // under the job. A panic leaves a message behind.
            Err(RecvTimeoutError::Disconnected) => Err(self.gone()),
        }
    }

    /// Queues `job` and returns immediately.
    ///
    /// The single send path, and the reason it never blocks: some callers are
    /// themselves *on a page thread with JavaScript on the stack* (the
    /// `window.open` hook, a `WindowProxy` op), where any wait is one the
    /// `ScriptBudget` cannot interrupt. A failure is classified through
    /// [`PageHandle::gone_now`] rather than [`PageHandle::gone`] — the latter
    /// polls for up to `CRASH_REPORT_GRACE`, so a driver draining a queue
    /// against a closed page would pay that per call. Anything the send path
    /// grows later (accounting, refusal, tracing) lands here rather than in
    /// four hand-rolled `cmd_tx.send` sites.
    fn send(&self, job: PageJob) -> EngineResult<()> {
        self.0.cmd_tx.send(job).map_err(|_| self.gone_now())
    }

    /// Posts work without waiting for it. Errors only if the page is gone.
    pub fn post<F>(&self, f: F) -> EngineResult<()>
    where
        F: FnOnce(&Page) + Send + 'static,
    {
        self.send(PageJob::new(f))
    }

    /// [`PageHandle::post`] for a control job — one that runs at whatever wait
    /// point receives it, including inside a navigation.
    fn post_control(&self, f: impl FnOnce(&Page) + Send + 'static) -> EngineResult<()> {
        self.send(PageJob::control(f))
    }

    /// [`PageHandle::gone`] without the wait: whatever the thread has published
    /// so far. Used where blocking is not allowed.
    fn gone_now(&self) -> EngineError {
        match self.0.crash.lock() {
            Ok(crash) => match crash.as_ref() {
                Some(message) => EngineError::Crashed(message.clone()),
                None => EngineError::Closed,
            },
            Err(_) => EngineError::Closed,
        }
    }

    /// Why a channel to the page failed — `Crashed` if the thread panicked,
    /// `Closed` if it simply stopped.
    ///
    /// The bounded wait is load-bearing. A panic drops the reply `Sender` as it
    /// unwinds, so `recv` reports `Disconnected` *before* `catch_unwind` has
    /// recorded the message: reading `crash` right away would report a real
    /// crash as an ordinary close. The thread sets `exited` only after storing
    /// the message, so waiting for that flag is what makes the two consistent.
    fn gone(&self) -> EngineError {
        let deadline = std::time::Instant::now() + CRASH_REPORT_GRACE;
        while !self.has_exited() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.gone_now()
    }

    // === The typed surface ===

    /// Navigates to `url` and waits as far as `wait`.
    ///
    /// The inner `Result` is the page's own: a navigation that fails (DNS,
    /// HTTP, a blocked address) is not an engine error. `JsError` is `!Send`
    /// — it can carry a `JsValue` — so it is rendered on the page thread,
    /// where the realm that owns those values still exists. The page raises
    /// such a failure as `JsError::Host`, whose `Display` is the network
    /// layer's text verbatim: this string becomes `Page.navigate.errorText`,
    /// which a driver compares against Chrome's exact `net::ERR_…` names.
    pub fn navigate(&self, url: &str, wait: WaitUntil) -> EngineResult<Result<(), String>> {
        let url = url.to_owned();
        self.with(move |page| page.navigate(&url, wait).map_err(|e| e.to_string()))
    }

    /// Reloads the current document, bypassing the HTTP cache.
    pub fn reload(&self, wait: WaitUntil) -> EngineResult<Result<(), String>> {
        self.with(move |page| page.reload(wait).map_err(|e| e.to_string()))
    }

    /// Cancels queued navigations, returning how many were dropped.
    ///
    /// A **control** call. An ordinary job is deferred while the page
    /// navigates, which is the one moment this method exists for: it would
    /// answer only once the load it was meant to cut short had finished. As a
    /// control job it answers at the page's first wait point instead. It clears
    /// one `RefCell` queue whose borrow is never held across a wait point, so it
    /// is sound at any of them.
    ///
    /// It is still not instant: a page inside a **blocking** document fetch
    /// services no job at all, so a caller must not treat this as interruptible
    /// on demand (`cdp::session::is_priority` explains what that rules out).
    pub fn stop_loading(&self) -> EngineResult<usize> {
        self.with_control(Page::stop_loading)
    }

    /// A snapshot of the session history.
    pub fn navigation_history(&self) -> EngineResult<NavigationHistory> {
        self.with(Page::navigation_history)
    }

    /// Every browsing context of this page, parents before their children.
    pub fn frame_tree(&self) -> EngineResult<Vec<FrameInfo>> {
        self.with(Page::frame_tree)
    }

    /// A backend handle for the `<iframe>` embedding `frame`.
    pub fn frame_owner_handle(&self, frame: FrameId) -> EngineResult<Option<u64>> {
        self.with(move |page| page.frame_owner_handle(frame))
    }

    /// Traverses to an absolute session-history index.
    pub fn navigate_to_history_entry(
        &self,
        index: usize,
        wait: WaitUntil,
    ) -> EngineResult<Result<(), String>> {
        self.with(move |page| {
            page.navigate_to_history_entry(index, wait)
                .map_err(|e| e.to_string())
        })
    }

    // === the remote object model (ADR-0030) ===

    /// Evaluates `source` in the page's main world.
    pub fn evaluate(
        &self,
        source: &str,
        options: EvaluateOptions,
    ) -> EngineResult<EvaluateOutcome> {
        let source = source.to_owned();
        self.with(move |page| page.evaluate(&source, &options))
    }

    /// Calls a function expression with `this` bound to a live handle.
    pub fn call_function_on(
        &self,
        declaration: String,
        object_id: Option<u64>,
        context_id: Option<u64>,
        args: Vec<CallArgument>,
        options: EvaluateOptions,
    ) -> EngineResult<Result<EvaluateOutcome, RemoteError>> {
        self.with(move |page| {
            page.call_function_on(&declaration, object_id, context_id, &args, &options)
        })
    }

    /// The own enumerable properties of a handle.
    pub fn get_properties(
        &self,
        object_id: u64,
        group: Option<String>,
    ) -> EngineResult<Result<Vec<PropertyDescriptor>, RemoteError>> {
        self.with(move |page| page.get_properties(object_id, group.as_deref()))
    }

    /// Settles a promise handle.
    pub fn await_promise(
        &self,
        object_id: u64,
        options: EvaluateOptions,
    ) -> EngineResult<Result<EvaluateOutcome, RemoteError>> {
        self.with(move |page| page.await_promise(object_id, &options))
    }

    /// Releases one handle. `false` if it was already gone.
    pub fn release_object(&self, object_id: u64) -> EngineResult<bool> {
        self.with(move |page| page.release_object(object_id))
    }

    /// Releases every handle in a group, returning how many.
    pub fn release_object_group(&self, group: String) -> EngineResult<usize> {
        self.with(move |page| page.release_object_group(&group))
    }

    /// Installs a global function that reports its argument to the embedder.
    pub fn add_binding(&self, name: String) -> EngineResult<Result<(), String>> {
        self.with(move |page| page.add_binding(&name))
    }

    /// Takes the binding payloads produced since the last call.
    pub fn drain_binding_calls(&self) -> EngineResult<Vec<oxidepage_page::BindingCall>> {
        self.with(Page::drain_binding_calls)
    }

    /// The id of the current document's execution context.
    ///
    /// A **control** call, and that is load-bearing. It reads a single `Cell`,
    /// so it is sound at any wait point — and it is read from a driver's event
    /// thread, which must never block: an ordinary job is deferred while the
    /// page navigates, parses, is suspended, or is parked in a dialog, so this
    /// would otherwise stall *every* event on that connection, for every
    /// target, for up to the command timeout.
    pub fn execution_context_id(&self) -> EngineResult<u64> {
        self.with_control(Page::execution_context_id)
    }

    /// Every live execution world of this page, main world first.
    ///
    /// A **control** call, for exactly the reason `execution_context_id` is:
    /// the CDP event thread reads it to re-announce contexts after a commit,
    /// and must never block behind a navigating page. It reads a `RefCell` and
    /// clones names and ids — no JS, no DOM, no layout — which stretches the
    /// "`Cell`s and channels only" convention for control jobs far enough that
    /// ADR-0033 D10 names it explicitly rather than leaving it to be
    /// rediscovered.
    pub fn worlds(&self) -> EngineResult<Vec<oxidepage_page::WorldInfo>> {
        self.with_control(Page::worlds)
    }

    /// Creates — or returns — the isolated world named `name`.
    pub fn create_isolated_world(
        &self,
        name: String,
    ) -> EngineResult<Result<oxidepage_page::WorldInfo, String>> {
        self.with(move |page| page.create_isolated_world(&name))
    }

    /// The same, for one browsing context.
    pub fn create_isolated_world_in(
        &self,
        frame: FrameId,
        name: String,
    ) -> EngineResult<Result<oxidepage_page::WorldInfo, String>> {
        self.with(move |page| page.create_isolated_world_in(frame, &name))
    }

    /// Evaluates in the world a context id names (main world when `None`).
    pub fn evaluate_in(
        &self,
        context_id: Option<u64>,
        source: String,
        options: EvaluateOptions,
    ) -> EngineResult<Result<EvaluateOutcome, String>> {
        self.with(move |page| page.evaluate_in(context_id, &source, &options))
    }

    /// Whether the page is suspended, waiting for a debugger to release it.
    ///
    /// Read from an atomic — no round trip, for the reason `PageInner::suspended`
    /// documents: the caller is a connection's event thread.
    #[must_use]
    pub fn is_suspended(&self) -> bool {
        self.0.suspended.load(Ordering::Acquire)
    }

    /// Turns this page's HTTP cache use off (or back on).
    pub fn set_cache_disabled(&self, disabled: bool) -> EngineResult<()> {
        self.with(move |page| page.set_cache_disabled(disabled))
    }

    /// Replaces `navigator.languages` and the `Accept-Language` header.
    pub fn set_languages(&self, languages: Vec<String>) -> EngineResult<Result<(), String>> {
        self.with(move |page| page.set_languages(languages).map_err(|e| e.to_string()))
    }

    /// Installs a binding in one named world, or in every world when `None`.
    pub fn add_binding_in(
        &self,
        name: String,
        world: Option<String>,
    ) -> EngineResult<Result<(), String>> {
        self.with(move |page| page.add_binding_in(&name, world.as_deref()))
    }

    /// Registers an init script for one named world, or the main world.
    pub fn add_init_script_for(&self, source: String, world: Option<String>) -> EngineResult<u64> {
        self.with(move |page| page.add_init_script_for(&source, world.as_deref()))
    }

    /// Registers a script to run at the start of every new document.
    pub fn add_init_script(&self, source: String) -> EngineResult<u64> {
        self.with(move |page| page.add_init_script(&source))
    }

    /// Removes an init script. `false` if no script has that id.
    pub fn remove_init_script(&self, id: u64) -> EngineResult<bool> {
        self.with(move |page| page.remove_init_script(id))
    }

    /// Bytes the page's JavaScript heap is currently using.
    pub fn js_heap_used(&self) -> EngineResult<i64> {
        self.with(Page::js_heap_used)
    }

    /// A retained response body, and whether it is text rather than bytes.
    pub fn response_body(
        &self,
        id: oxidepage_page::RequestId,
    ) -> EngineResult<Option<(Vec<u8>, bool)>> {
        self.with(move |page| page.response_body(id))
    }

    /// The current document URL.
    pub fn url(&self) -> EngineResult<String> {
        self.with(|page| page.dom().document_url().to_owned())
    }

    /// Loads an in-memory document as the current one.
    pub fn set_content(&self, html: &str) -> EngineResult<Result<(), String>> {
        let html = html.to_owned();
        self.with(move |page| page.load_html(&html).map_err(|e| e.to_string()))
    }

    /// Evaluates `source` and returns the result coerced to a string.
    pub fn eval_to_string(&self, source: &str) -> EngineResult<Result<String, String>> {
        let source = source.to_owned();
        self.with(move |page| page.eval_to_string(&source).map_err(|e| e.to_string()))
    }

    /// Runs the page's loop until it goes idle, or `budget` elapses.
    ///
    /// The command timeout is raised to `budget` plus a margin for this call:
    /// a settle that legitimately runs longer than the default timeout must not
    /// report the page as unresponsive.
    pub fn settle(&self, budget: Duration) -> EngineResult<()> {
        self.call_within(PageJob::new, budget + self.0.command_timeout, move |page| {
            page.settle(budget)
        })
    }

    /// Encodes a screenshot to PNG.
    ///
    /// Nested like [`PageHandle::set_content`], and for the same reason: the
    /// outer `Err` is "there is no page to ask", the inner one "the page could
    /// not answer". A layout that outran its budget (ADR-0037) produces the
    /// inner one — the bytes would be a picture of an empty document, and
    /// shipping them as a successful capture is the silent failure ADR-0015
    /// set out to remove.
    pub fn screenshot(&self, options: ScreenshotOptions) -> EngineResult<Result<Vec<u8>, String>> {
        self.with(move |page| {
            let bytes = page.screenshot_with(&options);
            match page.take_layout_abort() {
                Some(abort) => Err(abort.to_string()),
                None => Ok(bytes),
            }
        })
    }

    /// Renders the document to PDF. Nested exactly as
    /// [`PageHandle::screenshot`] is.
    pub fn pdf(
        &self,
        options: PdfOptions,
        paint: PaintOptions,
    ) -> EngineResult<Result<Vec<u8>, String>> {
        self.with(move |page| {
            let bytes = page.pdf(&options, &paint);
            match page.take_layout_abort() {
                Some(abort) => Err(abort.to_string()),
                None => Ok(bytes),
            }
        })
    }

    /// The document serialized back to HTML.
    pub fn content(&self) -> EngineResult<String> {
        self.with(Page::document_html)
    }

    /// The page's current layout viewport, including its device pixel ratio.
    pub fn viewport(&self) -> EngineResult<Viewport> {
        self.with(Page::viewport)
    }

    pub fn set_viewport(&self, viewport: Viewport) -> EngineResult<()> {
        self.with(move |page| page.set_viewport(viewport))
    }

    // === trusted input (ADR-0031) ===

    /// Synthesizes one trusted mouse event at viewport CSS coordinates.
    ///
    /// An ordinary job: it flushes layout and enters JS. A click that follows a
    /// link performs the navigation before answering, so this call is bounded by
    /// the same `command_timeout` [`PageHandle::navigate`] is — a slow load makes
    /// both time out, and treating a click as the more patient of the two would
    /// be incoherent.
    pub fn dispatch_mouse(&self, input: MouseInput) -> EngineResult<()> {
        self.with(move |page| page.dispatch_mouse(input))
    }

    /// Synthesizes a wheel tick at viewport CSS coordinates.
    pub fn dispatch_wheel(&self, input: WheelInput) -> EngineResult<()> {
        self.with(move |page| page.dispatch_wheel(input))
    }

    /// Synthesizes one trusted key event at the focused element.
    ///
    /// Takes the owned [`KeyEvent`] rather than `KeyInput`: a closure crossing
    /// the channel is `Send + 'static`, so the borrowed form is rebuilt here,
    /// on the page thread, from data the closure owns.
    pub fn dispatch_key(&self, event: KeyEvent) -> EngineResult<()> {
        self.with(move |page| {
            page.dispatch_key(KeyInput {
                kind: event.kind,
                key: &event.key,
                modifiers: event.modifiers,
                repeat: event.repeat,
                text: event.text.as_deref(),
                code: event.code.as_deref(),
                location: event.location,
            });
        })
    }

    /// Inserts text at the caret as one edit, with no key events — a paste or
    /// an IME commit.
    pub fn insert_text(&self, text: String) -> EngineResult<()> {
        self.with(move |page| page.insert_text(&text))
    }

    // === the node surface (ADR-0031) ===

    /// Describes a node and, within `depth`, its subtree.
    ///
    /// The double `Result` is the existing idiom: the outer one is transport and
    /// liveness, the inner one the page's own answer. Every [`NodeRef`] is
    /// resolved **inside** the closure that acts on it, so no id is carried
    /// across a job boundary where the document could commit under it.
    pub fn describe_node(
        &self,
        node: NodeRef,
        depth: i32,
        pierce: bool,
    ) -> EngineResult<Result<NodeDescription, RemoteError>> {
        self.with(move |page| {
            let id = page.resolve_node_ref(node)?;
            page.describe_node(id, depth, pierce)
        })
    }

    /// The document node, described to `depth`.
    pub fn document_description(
        &self,
        depth: i32,
        pierce: bool,
    ) -> EngineResult<Result<NodeDescription, RemoteError>> {
        self.with(move |page| page.describe_node(page.document_node(), depth, pierce))
    }

    /// Mints a remote object handle for a node — CDP's `DOM.resolveNode`.
    pub fn resolve_node(
        &self,
        node: NodeRef,
        context_id: Option<u64>,
        group: Option<String>,
    ) -> EngineResult<Result<RemoteObject, RemoteError>> {
        self.with(move |page| {
            let id = page.resolve_node_ref(node)?;
            page.node_object_in(id, context_id, group.as_deref())
        })
    }

    /// The backend handle for a node named some other way — CDP's
    /// `DOM.requestNode`.
    pub fn node_handle(&self, node: NodeRef) -> EngineResult<Result<u64, RemoteError>> {
        self.with(move |page| {
            let id = page.resolve_node_ref(node)?;
            page.node_handle(id)
        })
    }

    /// `querySelector` (`all = false`) or `querySelectorAll`, rooted at `root`,
    /// answered as backend handles.
    ///
    /// An invalid selector is the inner `Err` — it comes off the wire, so it is
    /// data, not a bug.
    pub fn query_selector(
        &self,
        root: NodeRef,
        selector: String,
        all: bool,
    ) -> EngineResult<Result<Vec<u64>, String>> {
        self.with(move |page| {
            let id = page
                .resolve_node_ref(root)
                .map_err(|error| error.to_string())?;
            let matches = if all {
                page.query_selector_all(id, &selector)?
            } else {
                page.query_selector(id, &selector)?.into_iter().collect()
            };
            matches
                .into_iter()
                .map(|node| page.node_handle(node).map_err(|e| e.to_string()))
                .collect()
        })
    }

    /// The four CSS boxes of a node — CDP's `DOM.getBoxModel`.
    pub fn box_quads(&self, node: NodeRef) -> EngineResult<Result<BoxQuads, RemoteError>> {
        self.with(move |page| {
            let id = page.resolve_node_ref(node)?;
            page.box_quads(id)
                .ok_or_else(|| RemoteError::WrongType(String::from("Could not compute box model.")))
        })
    }

    /// The painted quads of a node, one per client rect — CDP's
    /// `DOM.getContentQuads`.
    pub fn content_quads(
        &self,
        node: NodeRef,
    ) -> EngineResult<Result<Vec<[Point; 4]>, RemoteError>> {
        self.with(move |page| {
            let id = page.resolve_node_ref(node)?;
            Ok(page.content_quads(id))
        })
    }

    /// Scrolls a node into view if it is not already fully visible.
    pub fn scroll_into_view_if_needed(
        &self,
        node: NodeRef,
        rect: Option<Rect>,
    ) -> EngineResult<Result<bool, RemoteError>> {
        self.with(move |page| {
            let id = page.resolve_node_ref(node)?;
            Ok(page.scroll_into_view_if_needed(id, rect))
        })
    }

    /// `DOM.setFileInputFiles`: selects `paths` into an `<input type=file>`
    /// (ADR-0032 D11), firing trusted `input` and `change`.
    pub fn set_file_input_files(
        &self,
        node: NodeRef,
        paths: Vec<String>,
    ) -> EngineResult<Result<Result<(), String>, RemoteError>> {
        self.with(move |page| {
            let id = page.resolve_node_ref(node)?;
            Ok(page
                .set_file_input_files(id, &paths)
                .map_err(|error| error.to_string()))
        })
    }

    /// `Page.setInterceptFileChooserDialog` (ADR-0032 D12).
    ///
    /// An ordinary job, not a control one: it writes page state, and the bar
    /// for `control` is `Cell`s and channels only.
    pub fn set_intercept_file_chooser(&self, intercept: bool) -> EngineResult<()> {
        self.with(move |page| page.set_intercept_file_chooser(intercept))
    }

    /// The document's scroll position, viewport size and content extent.
    pub fn layout_metrics(&self) -> EngineResult<Result<LayoutMetrics, String>> {
        self.with(|page| {
            let metrics = page.layout_metrics();
            match page.take_layout_abort() {
                // All-zero metrics off a discarded box tree read as a real
                // measurement of a real page; a driver sizing a capture from
                // them would silently get nothing (ADR-0037 D7).
                Some(abort) => Err(abort.to_string()),
                None => Ok(metrics),
            }
        })
    }

    /// Event-loop counters — the diagnostic that proves the loop parks rather
    /// than spins.
    ///
    /// A *control* call: `Page::loop_stats` reads a `Cell` and nothing else,
    /// which is the bar, and a diagnostic that could only be read from a page
    /// healthy enough to service ordinary work would be unreadable in exactly
    /// the situations it exists for — a suspended page, or one spinning.
    pub fn loop_stats(&self) -> EngineResult<LoopStats> {
        self.with_control(Page::loop_stats)
    }

    /// Freezes the page: its timers, network delivery and script all stop, and
    /// only control work is serviced until [`PageHandle::resume`].
    ///
    /// A control call, so it takes effect even while the page is busy.
    pub fn suspend(&self) -> EngineResult<()> {
        // Set before the job is queued, so a reader between the two sees the
        // page as suspended rather than as still running — the safe direction:
        // it is about to be true, and `waitingForDebugger` reporting `true` a
        // moment early is what a driver is waiting to hear anyway.
        self.0.suspended.store(true, Ordering::Release);
        self.with_control(Page::suspend)
    }

    /// Releases a page created with [`NewPageOptions::suspended`].
    ///
    /// A control call, so it gets through a page that is otherwise servicing
    /// nothing.
    pub fn resume(&self) -> EngineResult<()> {
        let result = self.with_control(Page::resume);
        // Cleared *after* the job lands, and **only if it landed**: the mirror
        // of `suspend`'s asymmetry, in the same safe direction. Clearing on a
        // timed-out or dead page would report a still-frozen page as running,
        // and a driver reading `waitingForDebugger: false` never sends the
        // `runIfWaitingForDebugger` that would actually release it.
        if result.is_ok() {
            self.0.suspended.store(false, Ordering::Release);
        }
        result
    }

    /// Whether a dialog on this page is *held* for an explicit answer.
    ///
    /// False under [`DialogPolicy::Dismiss`] and [`DialogPolicy::Accept`],
    /// where the page answers itself and [`PageHandle::answer_dialog`] can only
    /// ever fail. A driver needs to know this before it promises a user that a
    /// dialog will wait — it is exactly CDP's `hasBrowserHandler`.
    #[must_use]
    pub fn awaits_dialog_answer(&self) -> bool {
        matches!(self.0.dialog_policy, DialogPolicy::Ask { .. })
    }

    /// The driver's handle on this page's request interception (ADR-0032 D2).
    ///
    /// Deliberately **not** a `PageHandle::with` round trip. A `Page.navigate`
    /// occupies its session's command lane for the whole load, and the document
    /// fetch it is blocked on is exactly the request a driver wants to pause —
    /// so a `continueRequest` that had to queue behind that navigation would
    /// deadlock against the command that would release it.
    ///
    /// `None` only for a page whose thread died before it could publish, which
    /// every other path already treats as a dead page.
    #[must_use]
    pub fn intercept(&self) -> Option<InterceptControl> {
        self.0.intercept.clone()
    }

    /// Whether this page has an event sink — the precondition for interception
    /// (ADR-0032). A page created through `engine` always has one; this exists
    /// so the protocol layer can state the requirement rather than assume it.
    pub fn has_event_sink(&self) -> EngineResult<bool> {
        self.with(Page::has_event_sink)
    }

    /// Releases every request this page holds paused, unmodified (ADR-0032 D7).
    ///
    /// Called when the interceptor goes away — a socket closing, a session
    /// detaching, a target being destroyed. `Continue` rather than `Fail`,
    /// which is what Chrome does and the safe answer: failing would break a
    /// page whose driver merely crashed.
    pub fn release_paused_requests(&self) {
        if let Some(intercept) = &self.0.intercept {
            intercept.release_all();
        }
    }

    /// Drops one protocol session's claim on interception, ending it only if
    /// that was the last claim (see `InterceptConfig::wanted_by`).
    pub fn release_interception_for(&self, session: &str) {
        if let Some(intercept) = &self.0.intercept {
            for id in intercept.release_sessions(&[session.to_owned()]) {
                intercept.send(oxidepage_page::InterceptCommand::release(id));
            }
        }
    }

    /// Whether a dialog is open on this page right now.
    ///
    /// Read from a shared atomic, so it stays truthful for a page that is
    /// parked in the dialog and can answer nothing else.
    #[must_use]
    pub fn dialog_pending(&self) -> bool {
        self.0.dialog_pending.load(Ordering::Acquire)
    }

    /// Answers the dialog the page is parked on under [`DialogPolicy::Ask`].
    ///
    /// Call it on [`PageEvent::DialogOpening`]. The answer channel is an
    /// unbuffered rendezvous by intent — an answer nobody asked for must not be
    /// queued up to release the *next* dialog — so this blocks briefly rather
    /// than using `try_send`: a driver that answers the instant it sees the
    /// event can easily get there before the page reaches its `recv`, and
    /// dropping the answer for being a few microseconds early would leave the
    /// dialog to time out.
    ///
    /// Returns [`EngineError::Timeout`] when no dialog is open.
    pub fn answer_dialog(&self, response: DialogResponse) -> EngineResult<()> {
        if !matches!(self.0.dialog_policy, DialogPolicy::Ask { .. })
            || !self.0.dialog_pending.load(Ordering::Acquire)
        {
            return Err(EngineError::Timeout);
        }
        match self.0.dialog_tx.send_timeout(response, DIALOG_ANSWER_GRACE) {
            Ok(()) => Ok(()),
            Err(SendTimeoutError::Timeout(_)) => Err(EngineError::Timeout),
            Err(SendTimeoutError::Disconnected(_)) => Err(self.gone()),
        }
    }

    /// The flag a sibling's `WindowProxy.closed` reads. Shared, so the answer
    /// needs no cross-thread round trip — which matters for the
    /// `while (!w.closed)` poll pages actually write.
    pub(crate) fn closed_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0.closed)
    }

    /// Applies one fire-and-forget [`WindowOp`] from an opener.
    ///
    /// Never blocks: the caller is the *opener's* page thread, with JavaScript
    /// on its stack and its own DOM borrowed.
    /// The two senders a sibling's `WindowProxy` needs, and nothing else.
    ///
    /// Deliberately *not* a `PageHandle` clone. The proxy lives as long as the
    /// opener's script keeps a reference to it, and a `PageHandle` holds the
    /// sibling's event `Receiver` — so capturing one would keep both ends of
    /// that channel alive, meaning it could never disconnect and its whole
    /// buffered backlog (up to `event_capacity`, console strings included)
    /// would outlive the page it belongs to.
    pub(crate) fn window_ops(&self) -> WindowOps {
        WindowOps {
            cmd_tx: self.0.cmd_tx.clone(),
            event_tx: self.0.event_tx.clone(),
        }
    }

    /// Asks the page to close and joins its thread, bounded by the browser's
    /// close timeout.
    ///
    /// Idempotent. On timeout the channel is simply dropped — the thread will
    /// notice and exit on its own — and the handle is marked closed, so a
    /// wedged page can never hold up a [`Browser::close`](crate::Browser::close).
    pub fn close(&self) {
        // Release first (ADR-0032 D7). A page parked on a *blocking* pause —
        // the top-level document's — services no job at all, not even a control
        // one: it is inside `run_blocking`'s `await_decision`, not at a wait
        // point. So a close arriving while the driver still holds that pause
        // would be answered only when the intercept timeout expired, and
        // `join_bounded` would give up first and *detach* the thread, leaking it
        // and its `Page`. Here rather than at each call site because every close
        // path has the problem — `Target.closeTarget`, `Browser.close`,
        // `BrowserContext::close` and the `Drop` that stands in for them.
        self.release_paused_requests();
        // A control job: it sets a `Cell`, so it runs even mid-navigation.
        let _ = self.post_control(Page::request_close);
        self.join_bounded();
    }

    pub(crate) fn join_bounded(&self) {
        let Ok(mut join) = self.0.join.lock() else {
            return;
        };
        let Some(handle) = join.take() else {
            return;
        };
        let deadline = std::time::Instant::now() + self.0.close_timeout;
        // `JoinHandle` has no timed join, so poll the flag the thread sets on
        // its way out and only then join — which is then guaranteed not to
        // block. A thread that ignores the close is detached rather than
        // waited on forever.
        while !self.has_exited() {
            if std::time::Instant::now() >= deadline {
                // Detached, not waited on: a page wedged in Rust (a dialog, a
                // synchronous document fetch) must never hold the browser open.
                self.0.closed.store(true, Ordering::Release);
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = handle.join();
    }
}

/// Spawns a page thread and blocks until its `Page` is built, so a construction
/// failure is an ordinary `Result` rather than a panic on a thread nobody is
/// watching.
pub(crate) fn spawn_page(
    id: PageId,
    options: NewPageOptions,
    net: SharedNetConfig,
    local_storage: SharedLocalStorage,
    context: BrowserContext,
    settings: PageSettings,
    // How long to wait for the page to be built: the driver's command timeout
    // for `new_page`, the far shorter script-blocking budget for `window.open`.
    launch_timeout: Duration,
) -> EngineResult<PageHandle> {
    let PageSettings {
        event_capacity,
        command_timeout,
        close_timeout,
        // The popup cap is the context's business — it is enforced before a
        // page is ever spawned.
        max_pages: _,
    } = settings;
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<PageJob>();
    let (event_tx, event_rx) = crossbeam_channel::bounded::<PageEvent>(event_capacity);
    // Rendezvous: an answer is only accepted while the page is actually parked
    // in `run_dialog`, never buffered for the next dialog.
    let (dialog_tx, dialog_rx) = crossbeam_channel::bounded::<DialogResponse>(0);
    // Filled in by the page thread once its `Page` exists: the flag belongs to
    // the page (it is *the page* that is parked), and it must be raised before
    // the announcement a driver answers on.
    let (flag_tx, flag_rx) = crossbeam_channel::bounded::<PageControls>(1);
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);

    let options_dialog_policy = options.resolved_dialog_policy();
    let closed = Arc::new(AtomicBool::new(false));
    let suspended = Arc::new(AtomicBool::new(options.suspended));
    let exited = Arc::new(AtomicBool::new(false));
    let crash = Arc::new(Mutex::new(None));

    let thread_closed = Arc::clone(&closed);
    let thread_exited = Arc::clone(&exited);
    let thread_crash = Arc::clone(&crash);
    let thread_events = event_tx.clone();
    let handle_events = event_tx.clone();
    let join = std::thread::Builder::new()
        .name(id.to_string())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                run_page_thread(
                    options,
                    net,
                    local_storage,
                    context,
                    cmd_rx,
                    event_tx,
                    DialogChannel { answers: dialog_rx },
                    flag_tx,
                    &ready_tx,
                );
            }));
            if let Err(payload) = outcome {
                let message = panic_message(&payload);
                if let Ok(mut slot) = thread_crash.lock() {
                    *slot = Some(message.clone());
                }
                let _ = thread_events
                    .send_timeout(PageEvent::Crashed { message }, TERMINAL_EVENT_GRACE);
            } else {
                // A short blocking send rather than `try_send`: this is the
                // event a driver ends its loop on, so losing it to a
                // momentarily-full bus would strand that loop. It is still
                // best-effort — a driver that never drains cannot be helped,
                // and `PageHandle::is_closed` is the authoritative answer.
                let _ = thread_events.send_timeout(PageEvent::Closed, TERMINAL_EVENT_GRACE);
            }
            thread_closed.store(true, Ordering::Release);
            // Last of all: `join_bounded` and `gone` wait on this, and the
            // crash message above must already be published when it is set.
            thread_exited.store(true, Ordering::Release);
        })
        .map_err(|e| EngineError::Launch(e.to_string()))?;

    // Bounded, because this is called from `HostHooks::open_window` on an
    // *opener's* page thread with JavaScript on its stack: an unbounded wait
    // there is unrecoverable — the `ScriptBudget` cannot fire (the block is in
    // Rust) and no control job can reach a thread that is not at a wait point.
    // `ready_rx` **first**. The thread publishes its dialog flag before it
    // reports readiness, so by the time this returns `Ok` the flag is already
    // sitting in its channel — while waiting on the flag first would park for
    // the whole timeout on any construction failure, since this function still
    // owns a `flag_tx` and the channel therefore never disconnects.
    match ready_rx.recv_timeout(launch_timeout) {
        Ok(Ok(())) => {}
        Ok(Err(message)) => {
            let _ = join.join();
            return Err(EngineError::Launch(message));
        }
        // Wedged inside `Page::new`. Detached rather than joined — joining is
        // the unbounded wait this timeout exists to avoid.
        Err(RecvTimeoutError::Timeout) => return Err(EngineError::Timeout),
        // The thread died before reporting: a panic inside `Page::new`.
        Err(RecvTimeoutError::Disconnected) => {
            let _ = join.join();
            let message = crash
                .lock()
                .ok()
                .and_then(|c| c.clone())
                .unwrap_or_else(|| "page thread exited during construction".to_owned());
            return Err(EngineError::Launch(message));
        }
    }

    let controls = flag_rx.try_recv().ok();
    let dialog_pending = controls.as_ref().map_or_else(
        || Arc::new(AtomicBool::new(false)),
        |c| Arc::clone(&c.dialog_flag),
    );
    let intercept = controls.map(|c| c.intercept);

    Ok(PageHandle(Arc::new(PageInner {
        id,
        cmd_tx,
        events: event_rx,
        event_tx: handle_events,
        dialog_tx,
        dialog_pending,
        suspended,
        intercept,
        dialog_policy: options_dialog_policy,
        closed,
        exited,
        crash,
        join: Mutex::new(Some(join)),
        command_timeout,
        close_timeout,
    })))
}

/// The whole life of a page thread: build the page, install the hooks that can
/// only exist here (they are `Rc`), then hand the thread to the command loop.
#[allow(clippy::too_many_arguments)]
fn run_page_thread(
    options: NewPageOptions,
    net: SharedNetConfig,
    local_storage: SharedLocalStorage,
    context: BrowserContext,
    cmd_rx: Receiver<PageJob>,
    events: Sender<PageEvent>,
    dialogs: DialogChannel,
    publish: Sender<PageControls>,
    ready_tx: &Sender<Result<(), String>>,
) {
    let dialog_policy = options.resolved_dialog_policy();
    let suspended = options.suspended;

    let page_options = PageOptions {
        url: options.url,
        viewport: options.viewport,
        navigator: options.navigator.unwrap_or_default(),
        screen: options.screen,
        script_budget: options.script_budget,
        layout_budget: options.layout_budget,
        lazy_images: options.lazy_images.unwrap_or(false),
        whole_document_visible: options.whole_document_visible.unwrap_or(false),
        download_path: options.download_path,
        // The policy lives on the shared pool; a per-page one would be ignored.
        policy: None,
        dialog_handler: None,
        net: Some(net),
        local_storage: Some(local_storage),
        ..PageOptions::default()
    };

    let page = match Page::new(page_options) {
        Ok(page) => page,
        Err(error) => {
            let _ = ready_tx.send(Err(error.to_string()));
            return;
        }
    };

    // Dropped counter and sink: `try_send` rather than `send`, because the sink
    // runs with JavaScript on the stack and blocking it would park the page on
    // a driver that stopped reading.
    let dropped = Rc::new(std::cell::Cell::new(0u64));
    {
        let events = events.clone();
        let dropped = Rc::clone(&dropped);
        page.set_event_sink(Some(Rc::new(move |record: PageRecord| {
            // A dialog announcement is the one record a driver *must* get: it
            // is the only thing that tells it to answer, and losing it parks
            // the page for the whole dialog timeout. Same treatment as the
            // terminal events, for the same reason.
            // Only under `Ask` does anyone have to act on the announcement.
            // Under the automatic policies it is informational, and blocking
            // the page thread for it — with JavaScript on the stack, inside
            // `run_dialog` — would tax every `alert()` on a page whose driver
            // is slow to drain the bus.
            //
            // A paused request is the second such record (ADR-0032 D2). The
            // announcement is the *only* thing that tells a driver the request
            // exists, so a dropped `Paused` holds it for the whole intercept
            // timeout with nobody able to release it.
            let load_bearing = (matches!(dialog_policy, DialogPolicy::Ask { .. })
                && matches!(record, PageRecord::DialogOpening(_)))
                || matches!(&record, PageRecord::Network { event, .. } if event.is_load_bearing());
            let event = PageEvent::from_record(record);
            if load_bearing {
                let _ = events.send_timeout(event, TERMINAL_EVENT_GRACE);
                return;
            }
            if events.try_send(event).is_err() {
                dropped.set(dropped.get() + 1);
                return;
            }
            // Report the backlog once there is room again, so a full bus is
            // visible rather than silent.
            let missed = dropped.replace(0);
            if missed > 0
                && events
                    .try_send(PageEvent::Dropped { count: missed })
                    .is_err()
            {
                dropped.set(missed);
            }
        })));
    }

    page.set_dialog_handler(Some(Rc::new(move |_request: &DialogRequest| {
        // The timeout comes off the policy itself, so there is no fallback to
        // pick: only `Ask` reaches the wait, and `Ask` carries its own bound.
        let DialogPolicy::Ask { timeout } = dialog_policy else {
            return dialog_policy.automatic().unwrap_or(DialogResponse::Dismiss);
        };
        // The page is parked here, with JS on the stack, until the driver
        // answers or the timeout expires. Both exits are bounded; the
        // `ScriptBudget` cannot help, since the block is in Rust (D11).
        //
        // `Page` raised its `dialog_open` flag before it announced this dialog,
        // so an `answer_dialog` racing that announcement finds the flag up and
        // blocks for the rendezvous instead of being refused.
        dialogs
            .answers
            .recv_timeout(timeout)
            .unwrap_or(DialogResponse::Dismiss)
    })));

    // `window.open` and `<a target=_blank>`: open a sibling into this page's
    // own context. Plain data in, plain data out — the hook runs with JS on the
    // stack, so it must not touch this page (ADR-0027 D12).
    page.set_open_window_handler(Some(Rc::new(
        move |request: &OpenWindowRequest| -> Option<OpenedWindow> { context.open_window(request) },
    )));

    let _ = publish.try_send(PageControls {
        dialog_flag: page.dialog_open_flag(),
        intercept: page.intercept(),
    });

    if suspended {
        page.suspend();
    }

    let _ = ready_tx.send(Ok(()));
    page.run_command_loop(cmd_rx);
    // `page` drops here — on its own thread, in its declared field order, which
    // is the teardown contract `Page` documents in place of a `Drop` impl.
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    oxidepage_page::panic_message(payload, "page thread panicked")
}
