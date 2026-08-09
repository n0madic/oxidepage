//! The push event bus: what a page tells its driver as it happens
//! (ADR-0027 D6).

use oxidepage_page::{
    ConsoleMessage, DialogEvent, DialogRequest, DownloadEvent, FileChooserEvent, NavigationEvent,
    PageRecord, ScriptError,
};

/// One thing a page did, delivered over [`PageHandle::events`].
///
/// [`PageHandle::events`]: crate::PageHandle::events
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PageEvent {
    Navigation(NavigationEvent),
    /// A nested browsing context was created or discarded (ADR-0035 D9).
    Frame(oxidepage_page::FrameEvent),
    Console(ConsoleMessage),
    Error(ScriptError),
    /// A dialog is open and **the page thread is parked on it right now**.
    ///
    /// Under [`DialogPolicy::Ask`](crate::DialogPolicy) this is the event to
    /// answer: call
    /// [`PageHandle::answer_dialog`](crate::PageHandle::answer_dialog) on
    /// receipt. Under the automatic policies it is informational and the answer
    /// has already been decided.
    DialogOpening(DialogRequest),
    /// A dialog the page opened, with the answer it got. Always follows a
    /// [`PageEvent::DialogOpening`].
    Dialog(DialogEvent),
    /// Page script called a function installed by
    /// [`PageHandle::add_binding`](crate::PageHandle::add_binding).
    Binding {
        name: String,
        payload: String,
        /// The execution context the call came from (ADR-0033 D10).
        context_id: u64,
    },
    /// One step of a network request's life (ADR-0030), and the browsing
    /// context that started it when one named itself (ADR-0035 D9).
    Network {
        event: oxidepage_page::NetworkEvent,
        frame: Option<oxidepage_page::FrameId>,
    },
    /// The answer to an evaluation that returned
    /// [`EvaluateOutcome::Deferred`](oxidepage_page::EvaluateOutcome::Deferred)
    /// (ADR-0034 D1).
    ///
    /// A driver matches `token` back to the command it left unanswered and
    /// replies then. Always arrives exactly once per deferred call: the page
    /// answers a settled promise, a budget-expired one, a navigation that
    /// destroyed its context, and its own close.
    AwaitSettled {
        token: oxidepage_page::AwaitToken,
        result: oxidepage_page::EvaluationResult,
    },
    /// An `<input type=file>` was activated with
    /// `Page.setInterceptFileChooserDialog` on (ADR-0032 D12).
    ///
    /// Unlike a dialog the page is **not** parked: answer with
    /// [`PageHandle::set_file_input_files`](crate::PageHandle::set_file_input_files)
    /// whenever convenient, or not at all.
    FileChooser(FileChooserEvent),
    /// A `Content-Disposition: attachment` navigation (ADR-0032 D13).
    ///
    /// Two per download: one as it begins, one when it has been written or
    /// refused. A refused download still reports both — a driver that asked for
    /// one and got silence cannot tell a refusal from a broken link.
    Download(DownloadEvent),
    /// A sibling called `w.focus()` on this page.
    ///
    /// Reported rather than acted on: focusing a browsing context means
    /// something only with a window manager, and there is none here. An
    /// embedder with tabs of its own can raise the right one (ADR-0027 D12).
    FocusRequested,
    /// The page's thread has exited. Always the last event on the channel.
    Closed,
    /// The page's thread panicked; the page is gone but the browser is not.
    Crashed {
        message: String,
    },
    /// The bus was full and `count` events were dropped rather than blocking
    /// the page thread.
    ///
    /// The pull streams this bus replaces drop the *oldest* entry on overflow;
    /// a channel can only refuse the newest. Both are bounded, they are
    /// bounded differently, and this marker is what keeps the difference from
    /// being silent.
    Dropped {
        count: u64,
    },
}

impl PageEvent {
    pub(crate) fn from_record(record: PageRecord) -> Self {
        match record {
            PageRecord::Navigation(event) => Self::Navigation(event),
            PageRecord::Frame(event) => Self::Frame(event),
            PageRecord::Console(message) => Self::Console(message),
            PageRecord::Error(error) => Self::Error(error),
            PageRecord::DialogOpening(request) => Self::DialogOpening(request),
            PageRecord::Dialog(event) => Self::Dialog(event),
            PageRecord::Binding {
                name,
                payload,
                context_id,
            } => Self::Binding {
                name,
                payload,
                context_id,
            },
            PageRecord::Network { event, frame } => Self::Network { event, frame },
            PageRecord::AwaitSettled { token, result } => Self::AwaitSettled { token, result },
            PageRecord::FileChooser(event) => Self::FileChooser(event),
            PageRecord::Download(event) => Self::Download(event),
        }
    }
}
