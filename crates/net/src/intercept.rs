//! Request interception: the pause point's vocabulary (ADR-0032 D1–D3).
//!
//! Nothing here knows what CDP is. Like [`crate::record`], this is a
//! network-level model with network-level names; the protocol crate renames it.
//! What it *is* is the one piece of state a driver thread and a page thread
//! share, so every type is `Send` and the shared half lives behind a `Mutex`.
//!
//! # Why the config is shared rather than messaged
//!
//! `Fetch.enable` and `Fetch.disable` write [`InterceptConfig`] directly, with
//! no round trip to the page thread, so a driver can turn interception on while
//! the page is mid-parse. The live [`InterceptConfig::paused`] set lives here
//! for the same reason and does a second job: it is what makes a resolution
//! command **idempotent**. `continueRequest` removes the id and only then sends,
//! so a second one — or the loser of two sessions both intercepting — answers
//! Chrome's `Invalid InterceptionId` instead of resurrecting a finished request.
//!
//! # Why the command channel is unbounded
//!
//! The rendezvous a dialog answer travels on (`bounded(0)`) would be wrong here.
//! An async-paused page is *not* parked — it may be mid-parse for seconds — so a
//! rendezvous send would block the shared CDP priority lane, and a method that
//! can block does not belong on that lane at all (ADR-0032 D4).

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use oxidepage_base::RequestId;

use crate::fetch::ResourceType;

/// How long a paused request waits for a decision before proceeding unmodified.
///
/// **Must stay strictly below the engine's command timeout** (30 s): a
/// `Page.navigate` whose document pause goes unanswered would otherwise report
/// `EngineError::Timeout` to the driver *while the page is still loading*, and
/// the driver would see a navigation fail that in fact succeeded moments later.
///
/// The timeout is a backstop, not the release mechanism — a driver that
/// detaches or drops its socket releases every paused request explicitly
/// (ADR-0032 D7). It only covers a driver that is wedged while holding the
/// socket open.
pub const DEFAULT_INTERCEPT_TIMEOUT: Duration = Duration::from_secs(20);

/// One `Fetch.enable` pattern.
#[derive(Clone, Debug)]
pub struct RequestPattern {
    /// Chrome's glob: `*` matches zero or more characters, `?` exactly one, and
    /// `\` escapes either. An empty pattern means `*`.
    pub url_pattern: String,
    /// Restricts the pattern to one kind of request, or `None` for any.
    pub resource_type: Option<ResourceType>,
}

impl Default for RequestPattern {
    fn default() -> Self {
        Self {
            url_pattern: String::from("*"),
            resource_type: None,
        }
    }
}

impl RequestPattern {
    #[must_use]
    pub fn matches(&self, url: &str, resource_type: ResourceType) -> bool {
        if let Some(wanted) = self.resource_type
            && wanted != resource_type
        {
            return false;
        }
        glob_matches(&self.url_pattern, url)
    }
}

/// Chrome's `urlPattern` glob.
///
/// Iterative rather than recursive, and deliberately so: the pattern comes
/// straight off an untrusted frame, and a backtracking recursive matcher over
/// `*?*?*?…` is a stack overflow a client can ask for. This form backtracks
/// through a single saved star position, which is linear in practice and cannot
/// recurse at all.
#[must_use]
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    // The last `*` seen and the text position it was matched against, so a
    // failed tail can retry with the star consuming one more character.
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some('?') => {
                p += 1;
                t += 1;
            }
            // A backslash escapes the next character, including `*`, `?` and
            // itself. A trailing backslash matches a literal backslash.
            Some('\\') => {
                let literal = pattern.get(p + 1).copied().unwrap_or('\\');
                if text[t] == literal {
                    p += 2.min(pattern.len() - p);
                    t += 1;
                } else if let Some((star_p, star_t)) = star {
                    p = star_p + 1;
                    t = star_t + 1;
                    star = Some((star_p, star_t + 1));
                } else {
                    return false;
                }
            }
            Some(&literal) if literal == text[t] => {
                p += 1;
                t += 1;
            }
            _ => {
                let Some((star_p, star_t)) = star else {
                    return false;
                };
                p = star_p + 1;
                t = star_t + 1;
                star = Some((star_p, star_t + 1));
            }
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// Which side of the connection asked for credentials.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthSource {
    Server,
    Proxy,
}

impl AuthSource {
    /// The request header credentials for this source go in.
    #[must_use]
    pub fn header(self) -> &'static str {
        match self {
            AuthSource::Server => "authorization",
            AuthSource::Proxy => "proxy-authorization",
        }
    }
}

/// A `WWW-Authenticate` / `Proxy-Authenticate` challenge the driver may answer.
///
/// Only `Basic` ever reaches here: [`crate::fetch::parse_auth_challenge`]
/// refuses Digest, NTLM and Negotiate by name rather than downgrading them
/// (ADR-0032 D8), so a driver is never asked for credentials that would then be
/// sent in a scheme this stack cannot compute.
#[derive(Clone, Debug)]
pub struct AuthChallenge {
    pub source: AuthSource,
    /// The origin that issued the challenge.
    pub origin: String,
    /// Always `"Basic"` today.
    pub scheme: String,
    pub realm: String,
}

/// How a driver answers an [`AuthChallenge`].
#[derive(Clone, Debug)]
pub enum AuthResponse {
    /// Send the credentials.
    Provide { username: String, password: String },
    /// Let the 401/407 through to the page, unanswered.
    Default,
    /// Cancel the request. Delivered as the challenge response itself, which is
    /// what a browser shows when a user dismisses the prompt.
    Cancel,
}

/// What a driver may rewrite on a continued request.
///
/// Every field is `None` for "leave it alone" — a `continueRequest` with no
/// members is the common case and must be byte-identical to no interception.
#[derive(Clone, Debug, Default)]
pub struct RequestOverrides {
    /// Re-validated at the pause boundary, not just when the fetch runs
    /// (ADR-0032 D5): a malformed or non-`http(s)` override answers
    /// `invalid_params` on the *command* rather than failing the request
    /// minutes later with a confusing network error.
    pub url: Option<String>,
    pub method: Option<String>,
    pub post_data: Option<Vec<u8>>,
    /// Replaces the request's headers wholesale, as CDP defines it.
    pub headers: Option<Vec<(String, String)>>,
}

/// A response a driver fabricated for a paused request.
#[derive(Clone, Debug)]
pub struct FulfilledResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A driver's decision about one paused request.
#[derive(Clone, Debug)]
pub enum InterceptCommand {
    /// Let it go, optionally rewritten.
    Continue {
        id: RequestId,
        overrides: Box<RequestOverrides>,
    },
    /// Answer it without touching the network.
    Fulfill {
        id: RequestId,
        response: Box<FulfilledResponse>,
    },
    /// Fail it with a `net::ERR_…`-shaped reason.
    Fail { id: RequestId, error: String },
    /// Answer an auth challenge.
    Auth {
        id: RequestId,
        response: AuthResponse,
    },
}

impl InterceptCommand {
    #[must_use]
    pub fn request_id(&self) -> RequestId {
        match self {
            InterceptCommand::Continue { id, .. }
            | InterceptCommand::Fulfill { id, .. }
            | InterceptCommand::Fail { id, .. }
            | InterceptCommand::Auth { id, .. } => *id,
        }
    }

    /// The release answer when the interceptor goes away (ADR-0032 D7).
    ///
    /// `Continue` unmodified, not `Fail`: it is what Chrome does, and failing
    /// would break a page whose driver merely crashed.
    #[must_use]
    pub fn release(id: RequestId) -> Self {
        InterceptCommand::Continue {
            id,
            overrides: Box::default(),
        }
    }
}

/// The interception state a driver thread and a page thread share.
#[derive(Debug, Default)]
pub struct InterceptConfig {
    /// `Fetch.enable` / `Fetch.disable`.
    pub enabled: bool,
    /// An empty list means "every request", which is what `Fetch.enable` with
    /// no `patterns` means.
    pub patterns: Vec<RequestPattern>,
    /// `Fetch.enable { handleAuthRequests }`. Accepted regardless of whether
    /// any auth is ever seen — Puppeteer sends it unconditionally.
    pub handle_auth: bool,
    /// `Network.emulateNetworkConditions { offline }` (ADR-0032 D9).
    pub offline: bool,
    /// `Network.emulateNetworkConditions { latency }`, applied *outside* the
    /// request timeout so it does not eat the request's own budget.
    pub latency: Duration,
    /// Requests announced as paused and not yet resolved. Membership is what
    /// makes a resolution idempotent.
    pub paused: HashSet<RequestId>,
}

impl InterceptConfig {
    /// Whether a request for `url` of this kind matches the driver's patterns.
    ///
    /// The scheme gate is **not** here: it belongs at the call site, next to the
    /// `file://`/`data:` early returns it mirrors (ADR-0032 D1).
    #[must_use]
    pub fn matches(&self, url: &str, resource_type: ResourceType) -> bool {
        if !self.enabled {
            return false;
        }
        if self.patterns.is_empty() {
            return true;
        }
        self.patterns
            .iter()
            .any(|pattern| pattern.matches(url, resource_type))
    }
}

/// A driver's handle on one page's interception.
///
/// Cloneable and `Send`: the CDP session lanes hold clones, and so does the page
/// (through its [`crate::NetService`]). The page holding one is load-bearing —
/// a `Receiver` whose only `Sender` lives on the driver side becomes
/// *permanently ready* in the page's `Select` the moment the driver goes away,
/// which turns the event loop's one park into a pegged core (ADR-0032 D2).
#[derive(Clone)]
pub struct InterceptControl {
    config: Arc<Mutex<InterceptConfig>>,
    tx: Sender<InterceptCommand>,
}

impl InterceptControl {
    /// Builds a control and the receiver the page's net service drains.
    #[must_use]
    pub fn new() -> (Self, Receiver<InterceptCommand>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (
            Self {
                config: Arc::new(Mutex::new(InterceptConfig::default())),
                tx,
            },
            rx,
        )
    }

    /// The shared config, recovering from a poisoned lock.
    ///
    /// Poisoning here means a page thread panicked mid-update. The state is a
    /// set of ids and a pattern list — there is no invariant a panic could have
    /// left half-written — and refusing to intercept for the life of the
    /// process is strictly worse than carrying on.
    pub fn config(&self) -> MutexGuard<'_, InterceptConfig> {
        self.config.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Turns interception on. Replaces any previous patterns.
    pub fn enable(&self, patterns: Vec<RequestPattern>, handle_auth: bool) {
        let mut config = self.config();
        config.enabled = true;
        config.patterns = patterns;
        config.handle_auth = handle_auth;
    }

    /// Turns interception off and reports the ids that must now be released.
    ///
    /// The caller sends the releases; this only clears the state, so the two
    /// cannot interleave with a request pausing in between.
    pub fn disable(&self) -> Vec<RequestId> {
        let mut config = self.config();
        config.enabled = false;
        config.patterns.clear();
        config.handle_auth = false;
        config.paused.drain().collect()
    }

    /// Every currently paused id, leaving them paused.
    #[must_use]
    pub fn paused_ids(&self) -> Vec<RequestId> {
        self.config().paused.iter().copied().collect()
    }

    /// Claims `id` for resolution: `true` exactly once per pause.
    ///
    /// The whole of the idempotence contract. A `continueRequest` that loses
    /// this race must answer `Invalid InterceptionId` rather than send.
    #[must_use]
    pub fn claim(&self, id: RequestId) -> bool {
        self.config().paused.remove(&id)
    }

    /// Queues a decision for the page thread. Never blocks (the channel is
    /// unbounded), which is what lets these run on the CDP priority lane.
    pub fn send(&self, command: InterceptCommand) {
        let _ = self.tx.send(command);
    }

    /// Claims `id` and sends `command`, reporting whether the claim succeeded.
    #[must_use]
    pub fn resolve(&self, command: InterceptCommand) -> bool {
        if !self.claim(command.request_id()) {
            return false;
        }
        self.send(command);
        true
    }

    /// The interceptor has gone away: stop intercepting, and release everything
    /// it was holding (ADR-0032 D7).
    ///
    /// **Turning interception off is half the job**, and the half that is easy
    /// to miss. Releasing the current pauses without clearing `enabled` leaves
    /// the page pausing every *subsequent* request with nobody left to answer —
    /// so each one waits out the full timeout, and each announcement blocks the
    /// page briefly on an event bus no one is draining. A page whose driver
    /// merely closed its socket would grind rather than carry on.
    pub fn release_all(&self) {
        for id in self.disable() {
            self.send(InterceptCommand::release(id));
        }
    }

    /// `Network.emulateNetworkConditions`' two honest members.
    pub fn set_conditions(&self, offline: bool, latency: Duration) {
        let mut config = self.config();
        config.offline = offline;
        config.latency = latency;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_glob_matches_the_way_chrome_documents_it() {
        assert!(glob_matches("*", "http://example.com/a"));
        assert!(glob_matches("", "http://example.com/a"));
        assert!(glob_matches(
            "http://example.com/*",
            "http://example.com/a/b"
        ));
        assert!(!glob_matches(
            "http://example.com/*",
            "https://example.com/a"
        ));
        assert!(glob_matches("*.png", "http://x/y/z.png"));
        assert!(!glob_matches("*.png", "http://x/y/z.png?q"));
        assert!(glob_matches("http://x/?", "http://x/a"));
        assert!(!glob_matches("http://x/?", "http://x/ab"));
        assert!(glob_matches("*a*b*c*", "zzazzbzzczz"));
        assert!(!glob_matches("*a*b*c*", "zzazzczzbzz"));
    }

    #[test]
    fn a_backslash_escapes_a_wildcard() {
        assert!(glob_matches(r"a\*b", "a*b"));
        assert!(!glob_matches(r"a\*b", "axxb"));
        assert!(glob_matches(r"a\?b", "a?b"));
        assert!(!glob_matches(r"a\?b", "axb"));
    }

    #[test]
    fn a_pathological_pattern_does_not_recurse() {
        // The pattern comes off an untrusted frame. A recursive backtracking
        // matcher overflows the stack on this; the iterative one answers.
        let pattern = "*?".repeat(200);
        let text = "a".repeat(1000);
        let _ = glob_matches(&pattern, &text);
    }

    #[test]
    fn a_resource_type_narrows_a_pattern() {
        let pattern = RequestPattern {
            url_pattern: String::from("*"),
            resource_type: Some(ResourceType::Image),
        };
        assert!(pattern.matches("http://x/a.png", ResourceType::Image));
        assert!(!pattern.matches("http://x/a.png", ResourceType::Document));
    }

    #[test]
    fn no_patterns_means_every_request() {
        let (control, _rx) = InterceptControl::new();
        control.enable(Vec::new(), false);
        assert!(control.config().matches("http://x/", ResourceType::Other));
    }

    #[test]
    fn a_disabled_config_matches_nothing() {
        let (control, _rx) = InterceptControl::new();
        assert!(!control.config().matches("http://x/", ResourceType::Other));
    }

    #[test]
    fn a_pause_can_be_claimed_exactly_once() {
        let (control, rx) = InterceptControl::new();
        let id = RequestId::from_parts(7, oxidepage_base::id::FIRST_GENERATION);
        control.config().paused.insert(id);

        assert!(control.resolve(InterceptCommand::release(id)));
        // The second caller — a driver's retry, or the loser of two sessions
        // both intercepting — must be told, not silently served.
        assert!(!control.resolve(InterceptCommand::release(id)));
        assert_eq!(rx.len(), 1, "exactly one decision reached the page");
    }

    #[test]
    fn disable_reports_everything_it_released() {
        let (control, _rx) = InterceptControl::new();
        let a = RequestId::from_parts(1, oxidepage_base::id::FIRST_GENERATION);
        let b = RequestId::from_parts(2, oxidepage_base::id::FIRST_GENERATION);
        control.enable(Vec::new(), true);
        control.config().paused.extend([a, b]);

        let mut released = control.disable();
        released.sort_by_key(|id| id.index());
        assert_eq!(released, vec![a, b]);
        assert!(!control.config().enabled);
        assert!(!control.config().handle_auth);
    }
}
