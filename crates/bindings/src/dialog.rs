//! `window.alert` / `confirm` / `prompt`: what the page asked, and how the
//! embedder answered.
//!
//! These three return a value *inline to JS*, so they cannot be a drained task
//! source the way navigation is — the answer has to come back while the
//! calling script is still on the stack. The hook
//! ([`HostHooks::run_dialog`](crate::HostHooks::run_dialog)) is therefore
//! synchronous, which also gives HTML's "pause the page" for free: the event
//! loop never regains control while the dialog is open, so no timer, frame
//! callback or network event can interleave.
//!
//! The shapes are CDP's, so a driver layer renames rather than translates:
//! [`DialogRequest`] is `Page.javascriptDialogOpening` and [`DialogResponse`]
//! is `Page.handleJavaScriptDialog { accept, promptText }`.

use std::rc::Rc;

/// Which of the three prompts the page opened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DialogKind {
    Alert,
    Confirm,
    Prompt,
}

impl DialogKind {
    /// The method name — `Page.javascriptDialogOpening`'s `type`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DialogKind::Alert => "alert",
            DialogKind::Confirm => "confirm",
            DialogKind::Prompt => "prompt",
        }
    }
}

/// What the page is asking.
///
/// Plain data by construction: the handler runs with JavaScript on the stack
/// and `RefCell` borrows held, so it must not be able to reach back into the
/// page. See [`DialogHandler`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DialogRequest {
    pub kind: DialogKind,
    /// Already coerced to a string per WebIDL.
    pub message: String,
    /// `prompt`'s pre-filled text; empty for the other two kinds.
    pub default_value: String,
    /// The URL of the document that raised the dialog.
    pub url: String,
}

/// The embedder's answer.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum DialogResponse {
    /// The default when no handler is installed, and what both Puppeteer and
    /// Playwright do with no `dialog` listener attached: `alert` returns,
    /// `confirm` → `false`, `prompt` → `null`.
    #[default]
    Dismiss,
    /// Accept. `confirm` → `true`; `prompt` → the page's own default text.
    Accept,
    /// Accept, and type this into a `prompt`. `alert` and `confirm` ignore
    /// the text.
    AcceptWith(String),
}

impl DialogResponse {
    /// Whether the dialog was accepted — `confirm`'s return value, and CDP's
    /// `handleJavaScriptDialog.accept`.
    #[must_use]
    pub fn accepted(&self) -> bool {
        !matches!(self, DialogResponse::Dismiss)
    }
}

/// One entry in the dialog stream: the ask and the answer together, so the
/// record says what the page actually observed and not merely what it wanted.
#[derive(Clone, PartialEq, Debug)]
pub struct DialogEvent {
    pub kind: DialogKind,
    pub message: String,
    pub default_value: String,
    pub response: DialogResponse,
    /// Unix-epoch milliseconds, from the page's monotonic time origin.
    pub timestamp: f64,
}

/// An embedder answer for `alert` / `confirm` / `prompt`.
///
/// **Runs with JavaScript on the stack**, under borrows of the page's dom,
/// style and layout. It receives plain data and returns plain data for exactly
/// that reason: it must not call back into the `Page`, which would re-enter
/// the JS context and double-borrow. It cannot do so by accident — reaching
/// the page requires capturing an `Rc<Page>`, which is a cycle — but an
/// embedder that goes out of its way (a `Weak<Page>` it upgrades) will panic
/// on a borrow, not corrupt anything.
///
/// Reinstalling the handler from inside the handler *is* allowed: the page
/// clones the `Rc` out of its slot and releases the borrow before calling.
pub type DialogHandler = Rc<dyn Fn(&DialogRequest) -> DialogResponse>;
