//! Errors an embedder can get back from a page that lives on another thread.

use std::fmt;

/// Why a call on a [`PageHandle`](crate::PageHandle) could not be answered.
///
/// Nothing here is a *page* error — a script that throws, a navigation that
/// 404s, a screenshot of an empty document all come back as ordinary
/// `Result`s from the underlying `Page` API. These are the failures that exist
/// only because the page is on another thread.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum EngineError {
    /// The page is closed: its thread has exited, or a close was requested.
    Closed,
    /// The page thread panicked. The page is gone; the browser is not.
    Crashed(String),
    /// The page did not answer within the configured command timeout. It is
    /// still alive — a page parked in a modal dialog or inside a synchronous
    /// document fetch legitimately cannot answer yet.
    Timeout,
    /// A page could not be created.
    Launch(String),
    /// Script asked for a page and the context refused: it is at
    /// [`BrowserOptions::max_pages_per_context`], the popup blocker.
    ///
    /// Distinct from [`EngineError::Closed`] because the two mean opposite
    /// things about the context — one is shut down, the other is open and
    /// working — and because this is the refusal worth logging and testing.
    ///
    /// [`BrowserOptions::max_pages_per_context`]: crate::BrowserOptions::max_pages_per_context
    PopupBlocked,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("page is closed"),
            Self::Crashed(message) => write!(f, "page thread panicked: {message}"),
            Self::Timeout => f.write_str("page did not answer within the command timeout"),
            Self::Launch(message) => write!(f, "could not launch page: {message}"),
            Self::PopupBlocked => f.write_str("popup blocked: context is at its page limit"),
        }
    }
}

impl std::error::Error for EngineError {}

/// The result of a call routed to a page thread.
pub type EngineResult<T> = Result<T, EngineError>;
