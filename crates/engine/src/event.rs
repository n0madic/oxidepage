//! The push event bus: what a page tells its driver as it happens
//! (ADR-0027 D6).

use oxidepage_page::{
    ConsoleMessage, DialogEvent, DialogRequest, NavigationEvent, PageRecord, ScriptError,
};

/// One thing a page did, delivered over [`PageHandle::events`].
///
/// [`PageHandle::events`]: crate::PageHandle::events
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PageEvent {
    Navigation(NavigationEvent),
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
            PageRecord::Console(message) => Self::Console(message),
            PageRecord::Error(error) => Self::Error(error),
            PageRecord::DialogOpening(request) => Self::DialogOpening(request),
            PageRecord::Dialog(event) => Self::Dialog(event),
        }
    }
}
