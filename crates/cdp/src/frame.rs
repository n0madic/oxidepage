//! One frame: what document it is showing, and which load produced it.
//!
//! Split out of [`target`](crate::target)'s entry, where this state used to sit
//! inline, because stage 11 (real iframes) is where the split starts to matter.
//! Today a target *is* one frame, so "the loader of this target" and "the loader
//! of this frame" name the same string; with real iframes a target owns a tree
//! and `loader_id`/`pending_loader` stop being properties of the target — every
//! frame commits its own documents. Keeping the whole of a frame's state in one
//! type now makes that step a change of *ownership*, a target growing a tree of
//! [`Frame`]s instead of holding exactly one, rather than a rewrite of the
//! loader bookkeeping that ADR-0032 D6a pinned down.
//!
//! Until then there is one frame per page and its id **is** the target id:
//! minting a second opaque id for a one-to-one mapping would be two things to
//! keep in sync for no gain.

use oxidepage_engine::page_api::RequestId;
use serde_json::json;

use crate::token::random_hex;

/// A frame's identity, its current document, and the loads behind both.
#[derive(Debug, Clone)]
pub struct Frame {
    /// CDP's `frameId` — the target id, until stage 11 gives frames ids of
    /// their own (see the module header).
    id: String,
    /// The URL of the document this frame is showing. `TargetInfo::url` mirrors
    /// it, because the top frame is the only frame there is.
    url: String,
    /// CDP's `loaderId`: an opaque id for *this document load*, not for the
    /// frame. Drivers use it to tell a fresh document from a same-document
    /// change, so it must be re-minted on every commit and must not be reused.
    loader_id: String,
    /// The loader minted for the navigation now in flight (ADR-0032 D6a), and
    /// whether a commit has adopted it yet.
    pending_loader: Option<PendingLoad>,
    /// How the last same-document navigation moved the URL, recorded at the one
    /// moment both URLs are known — see [`SameDocumentType`].
    same_document: SameDocumentType,
}

/// The navigation in flight, and the loader id its document will have.
///
/// **Minted when the navigation *starts*, not when it commits**, because that
/// is what Chrome does and what two separate Puppeteer mechanisms depend on:
///
/// * `Page.lifecycleEvent { name: "init" }` is the **only** event that sets
///   `frame._loaderId`, and `LifecycleWatcher` resolves a navigation only once
///   that value differs from the one it captured before the navigation. An
///   `init` carrying the *outgoing* loader leaves `page.goto()` hanging until
///   its own timeout — which is exactly what happened after a navigation that
///   failed without committing, since the committed loader had not moved.
/// * `isNavigationRequest` is `requestId === loaderId && type === 'Document'`,
///   so the document request's protocol id is this same string.
///
/// The **committed** loader ([`Frame::loader_id`]) only changes on a commit, so
/// a navigation that fails does not retire the current document's id — a driver
/// telling documents apart by loader must not see a phantom one.
#[derive(Debug, Clone)]
struct PendingLoad {
    /// The engine's id for the document request, once it has gone out. `None`
    /// between the navigation starting and its request being announced — and
    /// for a commit with no request at all (`about:blank`). Kept so
    /// `Network.getResponseBody` can map the substituted protocol id back.
    request: Option<RequestId>,
    loader: String,
    /// Set once a commit has taken this loader. A later commit with no
    /// navigation of its own must mint a fresh loader rather than re-use this.
    adopted: bool,
}

/// CDP's `Page.navigatedWithinDocument.navigationType`.
///
/// Chrome knows which API drove a same-document navigation. The engine's
/// `NavigationEvent` does not carry that — it records the milestone, not the
/// caller — so this is derived from the only thing that *is* available here:
/// the URL before the navigation and the URL after it. A difference confined to
/// the fragment is [`SameDocumentType::Fragment`]; anything else, including two
/// identical URLs, is [`SameDocumentType::HistoryApi`].
///
/// Two cases that heuristic gets wrong, both known rather than unnoticed:
///
/// * a `pushState`/`replaceState`/traversal that moves **only** the fragment
///   reads as `fragment`, where Chrome would say `historyApi`;
/// * `pushState`/`replaceState` produce no navigation milestone at all — they
///   move the document URL inside `bindings` without recording an event — so
///   `previous` is the URL of the last navigation *event*, not necessarily the
///   document's URL at the time. A fragment click after a `pushState` that
///   changed the path therefore compares against the pre-`pushState` URL and
///   reads as `historyApi`.
///
/// Chrome's third value, `other`, is never produced: nothing reaching this layer
/// distinguishes it from the two above, and inventing it would be a guess.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SameDocumentType {
    /// The URL moved within one document by its fragment alone.
    ///
    /// The default because it is the only kind the engine emits without a
    /// history API in play; nothing reads the value before a same-document
    /// navigation has set it.
    #[default]
    Fragment,
    HistoryApi,
}

impl SameDocumentType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fragment => "fragment",
            Self::HistoryApi => "historyApi",
        }
    }

    /// Classifies a same-document navigation from the two URLs alone — see the
    /// type's documentation for what that can and cannot tell apart.
    fn between(previous: &str, next: &str) -> Self {
        let (Ok(mut previous), Ok(mut next)) = (url::Url::parse(previous), url::Url::parse(next))
        else {
            // A URL that does not parse says nothing about fragments, and
            // claiming a fragment change would be the stronger, less honest
            // answer of the two.
            return Self::HistoryApi;
        };
        if previous.fragment() == next.fragment() {
            return Self::HistoryApi;
        }
        previous.set_fragment(None);
        next.set_fragment(None);
        if previous == next {
            Self::Fragment
        } else {
            Self::HistoryApi
        }
    }
}

impl Frame {
    /// A frame showing `url`, with a loader for the document it already has.
    #[must_use]
    pub fn new(id: String, url: String) -> Self {
        Self {
            id,
            url,
            loader_id: random_hex(),
            pending_loader: None,
            same_document: SameDocumentType::default(),
        }
    }

    /// The frame's id — the target id, until stage 11.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn set_url(&mut self, url: &str) {
        self.url = url.to_owned();
    }

    /// The id of the document currently **committed** in this frame.
    ///
    /// Unchanged by a navigation that fails, which is what keeps a driver from
    /// seeing a phantom document. For the loader a *loading* document will have,
    /// see [`Frame::loading_loader_id`].
    #[must_use]
    pub fn loader_id(&self) -> &str {
        &self.loader_id
    }

    /// The loader every event of the load in flight belongs to: the pending one
    /// if a navigation has started and not yet committed, else the committed
    /// one.
    ///
    /// This is what `Page.lifecycleEvent` must carry. `init` is the only event
    /// that sets Puppeteer's `frame._loaderId`, and `LifecycleWatcher` resolves
    /// a navigation only when that value has *changed* — so an `init` carrying
    /// the outgoing loader hangs `page.goto()` outright.
    #[must_use]
    pub fn loading_loader_id(&self) -> &str {
        match &self.pending_loader {
            Some(pending) if !pending.adopted => &pending.loader,
            _ => &self.loader_id,
        }
    }

    /// Mints the loader for a navigation that is *starting*, and returns it.
    ///
    /// Called on `NavigationEventKind::Started`, which is what makes the `init`
    /// lifecycle event carry the new document's loader — see [`PendingLoad`].
    pub fn begin_navigation(&mut self) -> String {
        let loader = random_hex();
        self.pending_loader = Some(PendingLoad {
            request: None,
            loader: loader.clone(),
            adopted: false,
        });
        loader
    }

    /// Commits the pending loader, or mints one for a commit that had no
    /// navigation of its own.
    ///
    /// Called on a *cross-document* commit only. A same-document navigation
    /// keeps its loader, which is exactly the distinction a driver reads it for.
    pub fn commit_loader(&mut self) -> String {
        let loader = match &mut self.pending_loader {
            Some(pending) if !pending.adopted => {
                pending.adopted = true;
                pending.loader.clone()
            }
            _ => random_hex(),
        };
        self.loader_id = loader.clone();
        loader
    }

    /// Drops the pending loader of a navigation that never committed.
    ///
    /// Only if it is still unadopted: a `Failed` that follows a *committed*
    /// navigation (a subresource giving up, a later same-document step) must
    /// leave the current document's loader alone.
    pub fn abandon_navigation(&mut self) {
        if self
            .pending_loader
            .as_ref()
            .is_some_and(|pending| !pending.adopted)
        {
            self.pending_loader = None;
        }
    }

    /// Records which request is the document request of the load in flight, so
    /// its protocol id can be the loader (ADR-0032 D6a).
    ///
    /// The loader is **not** minted here — `begin_navigation` already did, at
    /// the `Started` that preceded this. A request arriving with no pending
    /// navigation mints one anyway rather than losing the association.
    pub fn begin_document_load(&mut self, request: RequestId) -> String {
        match &mut self.pending_loader {
            Some(pending) if !pending.adopted => {
                pending.request = Some(request);
                pending.loader.clone()
            }
            _ => {
                let loader = random_hex();
                self.pending_loader = Some(PendingLoad {
                    request: Some(request),
                    loader: loader.clone(),
                    adopted: false,
                });
                loader
            }
        }
    }

    /// The substituted protocol id for `request`, iff it is the document
    /// request of the load now in flight (or the one that produced the current
    /// document).
    #[must_use]
    pub fn document_loader(&self, request: RequestId) -> Option<String> {
        let pending = self.pending_loader.as_ref()?;
        (pending.request == Some(request)).then(|| pending.loader.clone())
    }

    /// The inverse of [`Frame::document_loader`]: the engine request a
    /// substituted protocol id names, so `Network.getResponseBody` can answer
    /// for the document a driver just navigated to.
    #[must_use]
    pub fn request_for_loader(&self, loader: &str) -> Option<RequestId> {
        let pending = self.pending_loader.as_ref()?;
        (pending.loader == loader)
            .then_some(pending.request)
            .flatten()
    }

    /// Classifies a same-document navigation to `url` and remembers the answer.
    ///
    /// Must be called **before** the frame's URL moves: the classification is a
    /// statement about the difference between the two URLs, and [`Frame::url`]
    /// is the only record of the outgoing one.
    pub fn note_same_document(&mut self, url: &str) {
        self.same_document = SameDocumentType::between(&self.url, url);
    }

    /// The `navigationType` of the last same-document navigation.
    #[must_use]
    pub fn same_document_type(&self) -> SameDocumentType {
        self.same_document
    }

    /// CDP's `Page.Frame`.
    ///
    /// The loader and URL are passed rather than read off `self`, because the
    /// two callers want different ones: `Page.frameNavigated` describes the
    /// document *that commit* produced (the loading loader, the URL carried on
    /// the event), while `Page.getFrameTree` describes the document the frame
    /// has right now. Reading either off the registry at both call sites would
    /// make one navigation chasing another report the wrong document.
    #[must_use]
    pub fn json(&self, loader_id: &str, url: &str) -> serde_json::Value {
        json!({
            "id": self.id,
            "loaderId": loader_id,
            "url": url,
            // There is one browsing context per page until stage 11, so a frame
            // never has a parent and the origin is always the document's own.
            "securityOrigin": security_origin(url),
            "mimeType": "text/html",
        })
    }
}

/// CDP's opaque `frameId` for one of a page's browsing contexts.
///
/// **The top-level frame keeps the target id.** Minting a fresh one for it
/// would churn every existing driver expectation for no gain, and it is the
/// mapping the protocol has always reported.
///
/// A nested frame's id is *derived* from the target id and the engine's
/// generation-checked `FrameId` rather than drawn at random and stored. Derived
/// means there is no registry to keep in step with attach and detach, and the
/// id is stable across calls — which is what a driver comparing frame identity
/// needs. It is still opaque: a `FrameId` never leaves the process.
#[must_use]
pub fn frame_id_for(
    target_id: &str,
    frame: oxidepage_engine::page_api::FrameId,
    is_main: bool,
) -> String {
    if is_main {
        return target_id.to_owned();
    }
    // FNV-1a over the target id and the frame's index+generation. A collision
    // would need two frames of one page to hash alike, and the generation is
    // what keeps a reused slot from reviving a detached frame's id.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    };
    for byte in target_id.as_bytes() {
        mix(*byte);
    }
    for byte in frame.index().to_le_bytes() {
        mix(byte);
    }
    for byte in frame.generation().get().to_le_bytes() {
        mix(byte);
    }
    format!("{hash:016X}{:08X}", frame.index())
}

/// The serialized origin of `url`, or `"://"` for one that has none — which is
/// what Chrome reports for `about:blank`.
pub fn security_origin(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) if parsed.host_str().is_some() => {
            let scheme = parsed.scheme();
            let host = parsed.host_str().unwrap_or_default();
            match parsed.port() {
                Some(port) => format!("{scheme}://{host}:{port}"),
                None => format!("{scheme}://{host}"),
            }
        }
        _ => String::from("://"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_origin_drops_the_path_and_keeps_a_nondefault_port() {
        assert_eq!(
            security_origin("http://example.com/a/b?c"),
            "http://example.com"
        );
        assert_eq!(
            security_origin("http://127.0.0.1:8080/x"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            security_origin("https://example.com/"),
            "https://example.com"
        );
    }

    #[test]
    fn an_opaque_origin_is_reported_as_such() {
        // Chrome reports `://` for a document with no tuple origin. Reporting
        // the URL instead would let a driver believe `about:blank` is
        // same-origin with something.
        assert_eq!(security_origin("about:blank"), "://");
        assert_eq!(security_origin("data:text/html,hi"), "://");
        assert_eq!(security_origin("not a url"), "://");
    }

    #[test]
    fn a_frame_carries_the_ids_a_driver_matches_on() {
        let frame = Frame::new(String::from("t1"), String::from("http://example.com/"));
        let json = frame.json("l1", "http://example.com/");
        assert_eq!(json["id"], "t1");
        assert_eq!(json["loaderId"], "l1");
        assert_eq!(json["url"], "http://example.com/");
        assert_eq!(json["securityOrigin"], "http://example.com");
    }

    #[test]
    fn a_fragment_change_is_the_only_thing_reported_as_a_fragment_navigation() {
        let fragment = |a: &str, b: &str| SameDocumentType::between(a, b);
        assert_eq!(
            fragment("http://x/a", "http://x/a#one"),
            SameDocumentType::Fragment
        );
        assert_eq!(
            fragment("http://x/a#one", "http://x/a#two"),
            SameDocumentType::Fragment
        );
        assert_eq!(
            fragment("http://x/a#one", "http://x/a"),
            SameDocumentType::Fragment
        );
        // The path moved, so whatever else happened it was not a fragment
        // navigation — this is the `pushState`-created history entry a
        // traversal lands back on.
        assert_eq!(
            fragment("http://x/a#one", "http://x/b#one"),
            SameDocumentType::HistoryApi
        );
        assert_eq!(
            fragment("http://x/a", "http://x/b"),
            SameDocumentType::HistoryApi
        );
        // Identical URLs: a `replaceState` with no URL, or a traversal back to
        // the entry the document already shows.
        assert_eq!(
            fragment("http://x/a#one", "http://x/a#one"),
            SameDocumentType::HistoryApi
        );
    }

    #[test]
    fn an_unparseable_url_is_not_claimed_to_be_a_fragment_change() {
        assert_eq!(
            SameDocumentType::between("not a url", "http://x/a#one"),
            SameDocumentType::HistoryApi
        );
    }

    #[test]
    fn a_failed_navigation_leaves_the_committed_loader_alone() {
        let mut frame = Frame::new(String::from("t1"), String::from("about:blank"));
        let committed = frame.loader_id().to_owned();
        let pending = frame.begin_navigation();
        assert_eq!(frame.loading_loader_id(), pending);
        frame.abandon_navigation();
        assert_eq!(frame.loading_loader_id(), committed);
        assert_eq!(frame.loader_id(), committed);
    }

    #[test]
    fn a_commit_adopts_the_loader_the_navigation_minted() {
        let mut frame = Frame::new(String::from("t1"), String::from("about:blank"));
        let pending = frame.begin_navigation();
        assert_eq!(frame.commit_loader(), pending);
        assert_eq!(frame.loader_id(), pending);
        // A second commit with no navigation of its own must not re-use it.
        assert_ne!(frame.commit_loader(), pending);
    }
}
