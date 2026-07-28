//! Answering `alert`/`confirm`/`prompt` from a thread that is not the page's
//! (ADR-0027 D11).
//!
//! A dialog parks the page thread inside `run_dialog`, with JavaScript on the
//! stack — that is HTML's "pause" behavior, and it is why the answer travels
//! on a channel of its own rather than on the command port. The page services
//! no ordinary job while parked, so an answer queued behind the commands would
//! deadlock against the very thing it is meant to release.
//!
//! Every wait here is bounded. The `ScriptBudget` cannot rescue this one: it
//! is enforced through the JS engine's interrupt callback, and the block is in
//! Rust. A driver that never answers, or that goes away mid-dialog, gets the
//! auto-dismiss the page would have applied with no handler at all.

use std::time::Duration;

use oxidepage_page::DialogResponse;

/// How a driver wants `alert`/`confirm`/`prompt` handled.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DialogPolicy {
    /// Auto-dismiss, exactly as a bare `Page` does with no handler attached —
    /// and what both Puppeteer and Playwright do with no `dialog` listener
    /// (ADR-0025).
    #[default]
    Dismiss,
    /// Auto-accept: `confirm` returns true, `prompt` returns its default.
    Accept,
    /// Emit [`PageEvent::Dialog`](crate::PageEvent::Dialog) and **park the page
    /// thread** until [`PageHandle::answer_dialog`](crate::PageHandle::answer_dialog)
    /// or `timeout`, whichever comes first.
    ///
    /// The page runs nothing at all for the whole wait — no timers, no
    /// network delivery, no commands. Keep `timeout` short enough that a
    /// driver bug is a delay rather than a hang.
    Ask { timeout: Duration },
}

/// Default cap on how long a page will park waiting for a dialog answer.
pub const DEFAULT_DIALOG_TIMEOUT: Duration = Duration::from_secs(30);

impl DialogPolicy {
    /// The answer this policy gives without asking anyone.
    pub(crate) fn automatic(self) -> Option<DialogResponse> {
        match self {
            Self::Dismiss => Some(DialogResponse::Dismiss),
            Self::Accept => Some(DialogResponse::Accept),
            Self::Ask { .. } => None,
        }
    }
}
