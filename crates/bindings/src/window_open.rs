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

/// Whether a `target` **keyword** names the calling browsing context outright.
///
/// Only `_self` now: with real nested contexts `_parent` and `_top` name a
/// *different* one whenever there is one, so they go through
/// [`resolve_target`] instead of being assumed to mean "here" (ADR-0035 D10).
///
/// The empty string is deliberately **not** included, because the two callers
/// disagree about it and HTML says they should: an `<a>` with an absent or
/// empty `target` navigates the current context, while `window.open` maps an
/// empty target to `_blank` ("If target is the empty string, then set target to
/// `_blank`"). Each call site applies its own rule before asking this.
#[must_use]
pub fn target_is_current(target: &str) -> bool {
    target.eq_ignore_ascii_case("_self")
}

/// The browsing context a `target` names, resolved against the realm doing the
/// navigating (ADR-0035 D10).
///
/// `_self` and an absent target are this context; `_parent` and `_top` walk the
/// frame tree, each falling back to this context when there is nowhere to go,
/// exactly as HTML says. A name is looked up over the page's contexts. `None`
/// means "not a context that exists" — `_blank`, or a name nothing answers to —
/// and the caller decides what to open.
#[must_use]
pub fn resolve_target(
    state: &std::rc::Rc<crate::state::FrameShared>,
    target: &str,
) -> Option<std::rc::Rc<crate::state::FrameShared>> {
    if target.is_empty() || target.eq_ignore_ascii_case("_self") {
        return Some(std::rc::Rc::clone(state));
    }
    if target.eq_ignore_ascii_case("_parent") {
        return Some(
            state
                .parent_frame()
                .and_then(|parent| state.global.frame_state(parent))
                .unwrap_or_else(|| std::rc::Rc::clone(state)),
        );
    }
    if target.eq_ignore_ascii_case("_top") {
        let mut top = std::rc::Rc::clone(state);
        // Bounded by the frame-depth cap the page enforces; an owner chain
        // cannot cycle, but nothing here relies on that.
        for _ in 0..crate::state::MAX_FRAME_DESCENT {
            let Some(parent) = top
                .parent_frame()
                .and_then(|parent| top.global.frame_state(parent))
            else {
                break;
            };
            top = parent;
        }
        return Some(top);
    }
    if target.eq_ignore_ascii_case("_blank") {
        return None;
    }
    state.global.frame_by_name(state.frame(), target)
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
/// Two very different browsing contexts wear the same interface, and the split
/// is what decides how much a member can do:
///
/// * a **sibling** from `window.open` lives on another OS thread with its own
///   realm, so every member here is an atomic read or a fire-and-forget
///   message — a getter that blocked on a round trip would deadlock the first
///   time two pages opened each other (ADR-0027 D12);
/// * a **frame** of this page lives on *this* thread, so its members are real
///   and synchronous. No `JsValue` crosses even so: the proxy is an object of
///   the accessing realm and the child's globals stay unreachable
///   (ADR-0035 D4).
///
/// Deliberately holds no base URL. A `location` write resolves against the
/// accessing document's *current* URL, read at write time: the realm outlives
/// a navigation, so a URL captured when the proxy was made would send the
/// target somewhere the calling script never named.
pub(crate) enum WindowProxyData {
    Sibling(OpenedWindow),
    Frame(oxidepage_base::FrameId),
}
