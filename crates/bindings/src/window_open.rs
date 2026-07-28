//! `window.open`: the plain-data contract between script and the embedder
//! (ADR-0027 D12).
//!
//! Shaped exactly like [`DialogHandler`](crate::DialogHandler), and for the
//! same reason: the hook is called with JavaScript on the stack, under borrows
//! of the page's DOM and style. Data goes in, data comes out; nothing here can
//! reach back into the `Page`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What script asked for when it called `window.open`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OpenWindowRequest {
    /// The URL to open, already resolved against the opener's document. `None`
    /// for `window.open()` with no argument — `about:blank`.
    pub url: Option<String>,
    /// The `target` argument, or `_blank`. Named targets are not implemented,
    /// so this is carried for the embedder's information only.
    pub target: String,
    /// The `features` argument, uninterpreted.
    pub features: String,
    /// The URL of the document that called `open`.
    pub opener_url: String,
}

/// What the embedder gives back: a live handle on the new browsing context.
#[derive(Clone)]
pub struct OpenedWindow {
    /// Flipped by whoever closes the sibling. Read directly by
    /// `WindowProxy.closed`, so the getter never needs a cross-thread round
    /// trip — which matters, because it is read from a `while (!w.closed)` poll.
    pub closed: Arc<AtomicBool>,
    /// Fire-and-forget commands for the sibling. A plain callable rather than
    /// a channel so `bindings` needs no channel dependency for one sender.
    pub ops: Arc<dyn Fn(WindowOp) + Send + Sync>,
}

impl std::fmt::Debug for OpenedWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedWindow")
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Resolves `url` against `base`, keeping the raw string when `base` is not a
/// URL at all.
///
/// One rule for both window paths. They used to disagree: `window.open`
/// returned `None` on an unparseable base (blocking the popup) while
/// `WindowProxy.location =` fell back to the unresolved relative string
/// (navigating a sibling to something meaningless). Distinct from
/// `imp::request::resolve_url`, which resolves against the *document* and
/// throws — that is Fetch's rule, not HTML's window rule.
#[must_use]
pub fn resolve_against(base: &str, url: &str) -> String {
    url::Url::parse(base)
        .and_then(|base| base.join(url))
        .map_or_else(|_| url.to_owned(), |resolved| resolved.to_string())
}

/// Whether a `target` **keyword** names the *current* browsing context.
///
/// There is exactly one context per page here and it has no parent or opener,
/// so HTML's `_self`, `_parent` and `_top` all resolve to it — they navigate in
/// place and must never open anything. Only `_blank` and a name open a page.
///
/// The empty string is deliberately **not** included, because the two callers
/// disagree about it and HTML says they should: an `<a>` with an absent or
/// empty `target` navigates the current context, while `window.open` maps an
/// empty target to `_blank` ("If target is the empty string, then set target to
/// `_blank`"). Each call site applies its own rule before asking this.
#[must_use]
pub fn target_is_current(target: &str) -> bool {
    target.eq_ignore_ascii_case("_self")
        || target.eq_ignore_ascii_case("_parent")
        || target.eq_ignore_ascii_case("_top")
}

/// Something the opener asks of the window it opened.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WindowOp {
    /// `w.location = url` / `w.location.href = url`.
    Navigate(String),
    /// `w.close()`.
    Close,
    /// `w.focus()`. There is no window manager here, so the embedder is told
    /// rather than obeyed — which is what keeps this from being a silent no-op.
    Focus,
}

/// State behind one `WindowProxy` wrapper.
///
/// Deliberately holds no base URL. A `location` write resolves against the
/// opener's *current* document, read at write time: the realm outlives a
/// navigation, so a URL captured when `window.open` returned would send the
/// sibling somewhere the calling script never named.
pub(crate) struct WindowProxyData {
    pub(crate) window: OpenedWindow,
}
