//! The fetch pipeline (design doc §5.5): a hand-rolled `async fn` that
//! follows redirects, attaches/collects cookies, computes the referrer,
//! validates headers, decompresses the body, and applies the simple-request
//! CORS gate.
//!
//! Redirects re-enter the loop per hop, so the SSRF connector re-validates
//! every hop by construction (design §8) — there is no tower `Service`
//! composition to smuggle a validated connection across a redirect. Preflight
//! CORS (non-simple cross-origin requests) is deferred to Phase 10 and
//! rejected here.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use bytes::Bytes;
use http::header::{
    ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, CONTENT_ENCODING, CONTENT_LENGTH, COOKIE, LOCATION,
    ORIGIN, REFERER, SET_COOKIE, USER_AGENT, VARY,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request as HttpRequest, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use oxidepage_base::NetErrorKind;
use tokio::io::AsyncReadExt;
use url::Url;

use crate::cache::{CachePartition, HttpCache};
use crate::client::HttpClient;
use crate::cookies::{CookieJar, CookieSource, registrable_domain};
use crate::data;
use crate::error::{NetError, NetResult};
use crate::file;
use crate::policy::ResourcePolicy;

/// Cookie/credentials mode (Fetch `credentials`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Credentials {
    /// Never send or store cookies.
    Omit,
    /// Send/store cookies only for same-origin requests.
    #[default]
    SameOrigin,
    /// Always send/store cookies.
    Include,
}

/// Request mode (Fetch `mode`), gating cross-origin behavior.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RequestMode {
    /// Cross-origin requests fail.
    SameOrigin,
    /// CORS-checked; non-simple cross-origin requests need a preflight
    /// (rejected until Phase 10).
    #[default]
    Cors,
    /// No CORS check; cross-origin responses are opaque to script.
    NoCors,
    /// A top-level document navigation.
    Navigate,
}

/// Immutable user-agent-controlled headers applied to every HTTP request.
#[derive(Clone, Debug)]
pub struct RequestDefaults {
    user_agent: HeaderValue,
    accept_language: HeaderValue,
}

impl RequestDefaults {
    pub fn new(user_agent: &str, accept_language: &str) -> NetResult<Self> {
        if user_agent.is_empty() {
            return Err(NetError::protocol("User-Agent must not be empty"));
        }
        let user_agent = HeaderValue::from_str(user_agent)
            .map_err(|_| NetError::protocol("invalid User-Agent header value"))?;
        let accept_language = HeaderValue::from_str(accept_language)
            .map_err(|_| NetError::protocol("invalid Accept-Language header value"))?;
        Ok(Self {
            user_agent,
            accept_language,
        })
    }
}

impl Default for RequestDefaults {
    fn default() -> Self {
        Self {
            user_agent: HeaderValue::from_static("Mozilla/5.0 (compatible) OxidePage/0.1"),
            accept_language: HeaderValue::from_static("en-US"),
        }
    }
}

/// An engine-neutral network request (built by the bindings without any
/// dependency on hyper/http types).
pub struct NetRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub credentials: Credentials,
    pub mode: RequestMode,
    /// The referrer *source* URL (typically the document URL), or `None`.
    pub referrer: Option<String>,
    /// The initiator's origin (ASCII origin serialization), or `None` for a
    /// top-level navigation.
    pub initiator_origin: Option<String>,
    /// Skip the HTTP cache for this request.
    pub bypass_cache: bool,
}

impl NetRequest {
    /// A top-level document navigation (GET, credentialed, no referrer).
    #[must_use]
    pub fn navigation(url: impl Into<String>) -> Self {
        Self {
            method: "GET".to_owned(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            credentials: Credentials::Include,
            mode: RequestMode::Navigate,
            referrer: None,
            initiator_origin: None,
            bypass_cache: false,
        }
    }

    /// A top-level document navigation carrying a referrer and/or skipping the
    /// HTTP cache — a link click, a `location.href` write, `location.reload()`.
    ///
    /// The referrer is the URL of the document the navigation *left*, so it is
    /// passed in rather than derived: by the time the request is built the
    /// engine still has the outgoing document, but it is the caller that knows
    /// whether this navigation has a referrer at all (an embedder-driven
    /// `Page::navigate` does not).
    #[must_use]
    pub fn navigation_with(
        url: impl Into<String>,
        referrer: Option<String>,
        bypass_cache: bool,
    ) -> Self {
        Self {
            referrer,
            bypass_cache,
            ..Self::navigation(url)
        }
    }

    /// A form submission navigating the page (POST, credentialed).
    ///
    /// `RequestMode::Navigate` is what makes this work cross-origin: it is
    /// exempt from the CORS checks a script `fetch` would face, and it keeps
    /// the author-chosen `Content-Type` (`application/x-www-form-urlencoded`,
    /// `multipart/form-data`, `text/plain`) — a form POST to another origin is
    /// a normal, unpreflighted thing for a browser to do.
    #[must_use]
    pub fn form_navigation(
        url: impl Into<String>,
        body: Vec<u8>,
        content_type: String,
        referrer: Option<String>,
    ) -> Self {
        // A POST needs an `Origin` header, and `request_origin` derives it from
        // the initiator — so unlike a GET navigation this one names the
        // submitting document's origin, exactly as a browser does.
        let initiator_origin = referrer
            .as_deref()
            .and_then(|r| Url::parse(r).ok())
            .map(|u| u.origin().ascii_serialization());
        Self {
            method: "POST".to_owned(),
            headers: vec![("content-type".to_owned(), content_type)],
            body: Some(body),
            referrer,
            initiator_origin,
            ..Self::navigation(url)
        }
    }

    /// A subresource load initiated by a document (GET, credentialed).
    #[must_use]
    pub fn subresource(url: impl Into<String>, document_url: impl Into<String>) -> Self {
        let document_url = document_url.into();
        let initiator = Url::parse(&document_url)
            .ok()
            .map(|u| u.origin().ascii_serialization());
        Self {
            method: "GET".to_owned(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            credentials: Credentials::Include,
            mode: RequestMode::NoCors,
            referrer: Some(document_url),
            initiator_origin: initiator,
            bypass_cache: false,
        }
    }

    /// An ES module load initiated by a document (GET, CORS mode,
    /// same-origin credentials — the module fetch rules).
    #[must_use]
    pub fn module(url: impl Into<String>, document_url: impl Into<String>) -> Self {
        let document_url = document_url.into();
        let initiator = Url::parse(&document_url)
            .ok()
            .map(|u| u.origin().ascii_serialization());
        Self {
            method: "GET".to_owned(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            credentials: Credentials::SameOrigin,
            mode: RequestMode::Cors,
            referrer: Some(document_url),
            initiator_origin: initiator,
            bypass_cache: false,
        }
    }
}

/// A response's visibility class (Fetch response type), deciding how much of
/// the response script may observe.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ResponseType {
    /// Same-origin (or a navigation): fully visible.
    #[default]
    Basic,
    /// Cross-origin CORS: visible, but headers are pruned to the CORS-exposed
    /// set (done here in `net`).
    Cors,
    /// Cross-origin `no-cors`: status/headers/body are hidden from script.
    Opaque,
}

/// The head of a completed response.
#[derive(Debug)]
pub struct ResponseHead {
    pub status: u16,
    pub status_text: String,
    pub final_url: String,
    pub headers: Vec<(String, String)>,
    pub redirected: bool,
    /// The response's visibility class (basic / cors / opaque).
    pub response_type: ResponseType,
}

/// A fully-read response (head + decompressed body).
#[derive(Debug)]
pub struct FetchOutcome {
    pub head: ResponseHead,
    pub body: Bytes,
}

/// The shareable half of a [`FetchEngine`]: the connection pool and the HTTP
/// cache a whole browser has in common (design §7, ADR-0027 D7).
///
/// The budget counters are deliberately *not* here. `max_requests` and
/// `max_total_bytes` are documented per page, and a `FetchEngine` built over
/// shared parts still mints its own — sharing them would silently turn a
/// per-page bound into a browser-wide one.
pub struct SharedFetchParts {
    pub client: HttpClient,
    /// The policy `client`'s SSRF connector was built with.
    ///
    /// Carried **together with** the client rather than passed alongside it,
    /// because they must be the same policy: the connector enforces the address
    /// filter at connect time and this one is re-checked per redirect hop. A
    /// caller free to pair a `permissive_localhost` client with a strict policy
    /// could turn the private-address block into a no-op, with nothing to warn
    /// them. Only [`NetPool`](crate::NetPool) mints these, so the pairing has
    /// exactly one origin.
    pub(crate) policy: Arc<ResourcePolicy>,
    pub cache: Arc<Mutex<HttpCache>>,
    /// Isolation key for this engine's cache entries.
    pub partition: CachePartition,
}

/// The fetch engine: a client bound to a policy plus the page-shared cookie
/// jar, HTTP cache, and per-page resource budget counters. Cloneable (all
/// shared state is behind `Arc`), so `NetService` can hand a clone to each
/// spawned request and they share one cache/jar/budget.
#[derive(Clone)]
pub struct FetchEngine {
    pub client: HttpClient,
    pub policy: Arc<ResourcePolicy>,
    pub cookies: Arc<Mutex<CookieJar>>,
    cache: Arc<Mutex<HttpCache>>,
    /// Isolation key for this engine's entries in the (possibly shared) cache.
    partition: CachePartition,
    /// Cumulative request count (against [`ResourcePolicy::max_requests`]).
    request_count: Arc<AtomicU32>,
    /// Cumulative response bytes (against [`ResourcePolicy::max_total_bytes`]).
    total_bytes: Arc<AtomicU64>,
    request_defaults: RequestDefaults,
}

impl FetchEngine {
    pub fn new(policy: Arc<ResourcePolicy>, cookies: Arc<Mutex<CookieJar>>) -> NetResult<Self> {
        Self::new_with_defaults(policy, cookies, RequestDefaults::default())
    }

    pub fn new_with_defaults(
        policy: Arc<ResourcePolicy>,
        cookies: Arc<Mutex<CookieJar>>,
        request_defaults: RequestDefaults,
    ) -> NetResult<Self> {
        let client = HttpClient::new(Arc::clone(&policy))?;
        Ok(Self::with_shared(
            SharedFetchParts {
                client,
                policy,
                cache: Arc::new(Mutex::new(HttpCache::default())),
                partition: CachePartition::default(),
            },
            cookies,
            request_defaults,
        ))
    }

    /// Builds an engine over a connection pool and cache someone else owns.
    ///
    /// The policy travels *inside* `parts`, with the client it was used to
    /// build: the SSRF connector is baked into the client, so a pool built for
    /// one policy must never serve another (ADR-0004 D1), and making that a
    /// caller's responsibility would have been an invariant enforced only by a
    /// doc comment.
    #[must_use]
    pub fn with_shared(
        parts: SharedFetchParts,
        cookies: Arc<Mutex<CookieJar>>,
        request_defaults: RequestDefaults,
    ) -> Self {
        let SharedFetchParts {
            client,
            policy,
            cache,
            partition,
        } = parts;
        Self {
            client,
            policy,
            cookies,
            cache,
            partition,
            request_count: Arc::new(AtomicU32::new(0)),
            total_bytes: Arc::new(AtomicU64::new(0)),
            request_defaults,
        }
    }

    /// Charges one request against the per-page request-count budget.
    fn charge_request(&self) -> NetResult<()> {
        let n = self.request_count.fetch_add(1, Ordering::Relaxed) + 1;
        if n > self.policy.max_requests {
            return Err(NetError::blocked(format!(
                "per-page request budget exceeded ({} requests)",
                self.policy.max_requests
            )));
        }
        Ok(())
    }

    /// Charges `len` response bytes against the per-page cumulative-byte budget.
    fn charge_bytes(&self, len: u64) -> NetResult<()> {
        let total = self.total_bytes.fetch_add(len, Ordering::Relaxed) + len;
        if total > self.policy.max_total_bytes {
            return Err(NetError::new(
                NetErrorKind::Blocked,
                format!(
                    "per-page response byte budget exceeded ({} bytes)",
                    self.policy.max_total_bytes
                ),
            ));
        }
        Ok(())
    }

    /// Claims a slice of the cumulative byte budget *before* a body is streamed.
    ///
    /// Sizing the read from an unreserved `max_total_bytes - total_bytes` lets
    /// every concurrent fetch see the same headroom: with the 16-way fetch
    /// semaphore, sixteen responses each buffer up to `max_response_bytes` and
    /// the page's resident bytes overshoot the advertised budget many times over
    /// before the first `charge_bytes` lands. Reserving up front makes the
    /// budget an actual bound on bytes in flight; [`BudgetReservation`] refunds
    /// whatever the response did not use.
    fn reserve_bytes(&self, expected_len: Option<u64>) -> NetResult<BudgetReservation> {
        let budget = self.policy.max_total_bytes;
        let requested = expected_len
            .unwrap_or(self.policy.max_response_bytes)
            .min(self.policy.max_response_bytes);
        if requested == 0 {
            return Ok(BudgetReservation {
                total: Arc::clone(&self.total_bytes),
                outstanding: 0,
            });
        }
        let mut current = self.total_bytes.load(Ordering::Acquire);
        loop {
            let remaining = budget.saturating_sub(current);
            if remaining == 0 {
                return Err(NetError::new(
                    NetErrorKind::Blocked,
                    format!("per-page response byte budget exceeded ({budget} bytes)"),
                ));
            }
            let reserved = remaining.min(requested);
            match self.total_bytes.compare_exchange_weak(
                current,
                current + reserved,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(BudgetReservation {
                        total: Arc::clone(&self.total_bytes),
                        outstanding: reserved,
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Cumulative response bytes charged so far (test/introspection aid).
    #[must_use]
    pub fn total_charged_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// Runs a request to completion, following redirects. The whole operation
    /// (DNS + connect + every redirect hop + body read/decompress) is bounded
    /// by [`ResourcePolicy::request_timeout`]; exceeding it yields
    /// [`NetErrorKind::Timeout`] instead of hanging on a slow or silent server.
    pub async fn fetch(&self, request: NetRequest) -> NetResult<FetchOutcome> {
        let overall = self.policy.request_timeout;
        match tokio::time::timeout(overall, self.fetch_inner(request)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(NetError::new(
                NetErrorKind::Timeout,
                format!("request exceeded the {overall:?} time budget"),
            )),
        }
    }

    async fn fetch_inner(&self, request: NetRequest) -> NetResult<FetchOutcome> {
        let now = SystemTime::now();
        let mut url = Url::parse(&request.url)
            .map_err(|e| NetError::invalid_url(format!("{}: {e}", request.url)))?;

        // `file://` is not an HTTP flow.
        if url.scheme() == "file" {
            return self.load_file(&url).await;
        }
        // Neither is `data:`: the bytes are in the URL, so there is no address
        // to vet and no policy gate that could apply. Handling it here — above
        // the scheme gate, but outside the redirect loop below, which re-checks
        // `scheme_allowed` per hop — is what keeps an `http:` → `data:`
        // redirect a network error, as Fetch requires.
        if url.scheme() == "data" {
            return data_outcome(&url);
        }
        if !self.policy.scheme_allowed(url.scheme()) {
            return Err(NetError::blocked(format!(
                "scheme `{}` is not allowed",
                url.scheme()
            )));
        }

        let initiator = request
            .initiator_origin
            .as_deref()
            .and_then(|o| Url::parse(o).ok());
        let referrer_source = request.referrer.as_deref().and_then(|r| Url::parse(r).ok());
        let mut method = Method::from_bytes(request.method.to_ascii_uppercase().as_bytes())
            .map_err(|_| NetError::invalid_url(format!("invalid method `{}`", request.method)))?;
        let mut body = request.body.clone().unwrap_or_default();
        let mut redirected = false;
        let mut hops = 0u32;
        // A working copy of the script headers so a cross-origin redirect can
        // strip credential-bearing headers for the following hops.
        let mut headers = request.headers.clone();
        // Set once any redirect hop crosses origin: it taints the whole chain
        // so the final CORS gate applies even if the last hop lands back on the
        // initiator's origin.
        let mut cross_origin_taint = false;
        // Fetch §HTTP-redirect fetch: a cors-mode request that is redirected
        // across origins continues with an opaque origin, so later hops carry
        // `Origin: null` instead of naming the initiator.
        let mut origin_opaque = false;

        // HTTP cache (RFC 9111): only a plain same-origin GET with no body is
        // cacheable. The key/policy use the original request; a redirected
        // chain is never stored (its final URL would be lost on a hit), and
        // neither is a response carrying `Set-Cookie` (the cache stays out of
        // cookie correctness). Correctness never depends on it — a miss just
        // re-fetches.
        let cache_parts = (!request.bypass_cache
            && method == Method::GET
            && request.body.is_none()
            && !is_cross_origin(initiator.as_ref(), &url))
        .then(|| cache_request_parts(&url))
        .transpose()?;
        if let Some(parts) = &cache_parts {
            let hit = lock_recovering(&self.cache).get(self.partition, parts, now);
            if let Some(cached) = hit {
                self.charge_bytes(cached.body.len() as u64)?;
                return Ok(cached_outcome(cached, &url));
            }
        }

        loop {
            // Per-hop request-count budget: a redirect chain of N hops consumes
            // N units (each hop is a real network request that must be vetted).
            self.charge_request()?;

            // Per-hop scheme re-validation (a redirect may change scheme).
            if !self.policy.scheme_allowed(url.scheme()) {
                return Err(NetError::blocked(format!(
                    "redirect to disallowed scheme `{}`",
                    url.scheme()
                )));
            }

            let cross_origin = is_cross_origin(initiator.as_ref(), &url);
            cross_origin_taint |= cross_origin;
            if cross_origin {
                match request.mode {
                    RequestMode::SameOrigin => {
                        return Err(NetError::blocked(
                            "same-origin mode: cross-origin request blocked",
                        ));
                    }
                    RequestMode::Cors if !is_simple_request(&method, &headers) => {
                        return Err(NetError::blocked(
                            "CORS preflight required (deferred to Phase 10)",
                        ));
                    }
                    _ => {}
                }
            }

            let send_cookies = match request.credentials {
                Credentials::Omit => false,
                Credentials::Include => true,
                Credentials::SameOrigin => !cross_origin,
            };

            let origin = request_origin(&method, request.mode, initiator.as_ref(), origin_opaque);
            let hreq = self.build_request(
                &method,
                &url,
                &body,
                &headers,
                request.mode,
                referrer_source.as_ref(),
                origin.as_deref(),
                send_cookies,
                is_same_site(initiator.as_ref(), &url),
                now,
            )?;

            let resp = self.client.send_once(hreq).await?;
            let (parts, incoming) = resp.into_parts();

            if send_cookies {
                let mut jar = lock_recovering(&self.cookies);
                for value in parts.headers.get_all(SET_COOKIE).iter() {
                    if let Ok(s) = value.to_str() {
                        jar.set_cookie(&url, s, CookieSource::Http, now);
                    }
                }
            }

            // Redirect handling.
            let status = parts.status;
            if is_redirect(status)
                && let Some(location) = parts.headers.get(LOCATION)
            {
                if hops >= self.policy.max_redirects {
                    return Err(NetError::new(
                        NetErrorKind::TooManyRedirects,
                        format!("exceeded {} redirects", self.policy.max_redirects),
                    ));
                }
                let location = location
                    .to_str()
                    .map_err(|_| NetError::protocol("non-ASCII Location header"))?;
                let next = url.join(location).map_err(|e| {
                    NetError::invalid_url(format!("bad redirect target `{location}`: {e}"))
                })?;
                // A cross-origin redirect must not carry the initiator's
                // credentials onward (Fetch: strip `Authorization` /
                // `Proxy-Authorization` when the origin changes).
                if next.origin() != url.origin() {
                    strip_cross_origin_credentials(&mut headers);
                    if request.mode == RequestMode::Cors {
                        origin_opaque = true;
                    }
                }
                downgrade_method(status, &mut method, &mut body);
                url = next;
                hops += 1;
                redirected = true;
                // Drop the redirect's body and loop (per-hop SSRF re-check).
                drop(incoming);
                continue;
            }

            // Final response. Claim the read allowance from the cumulative byte
            // budget before streaming, so concurrent fetches cannot each buffer
            // against the same headroom; the unused remainder is refunded when
            // the reservation drops (including on an error path).
            // An unencoded response with a trustworthy Content-Length needs
            // only that many bytes reserved. Encoded or lengthless responses
            // keep the conservative per-response maximum because their
            // decoded size is not known before streaming.
            let is_encoded = parts
                .headers
                .get(CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    let value = value.trim();
                    !value.is_empty() && !value.eq_ignore_ascii_case("identity")
                });
            let expected_len = (!is_encoded)
                .then(|| {
                    parts
                        .headers
                        .get(CONTENT_LENGTH)?
                        .to_str()
                        .ok()?
                        .parse::<u64>()
                        .ok()
                })
                .flatten();
            let mut reservation = self.reserve_bytes(expected_len)?;
            let read_cap = reservation.cap();
            let raw = read_body(incoming, read_cap).await?;
            let body_bytes = self.maybe_decompress(&parts.headers, raw, read_cap).await?;
            reservation.commit(body_bytes.len() as u64);

            // A redirect chain that ever crossed origin taints the whole chain,
            // so the CORS gate applies even when the final hop lands back on the
            // initiator's origin.
            let cross_origin = cross_origin_taint;
            if cross_origin && request.mode == RequestMode::Cors {
                check_cors(&parts.headers, initiator.as_ref(), request.credentials)?;
            }
            let response_type = match request.mode {
                _ if !cross_origin => ResponseType::Basic,
                RequestMode::NoCors => ResponseType::Opaque,
                RequestMode::Cors => ResponseType::Cors,
                // SameOrigin cross-origin errored above; a navigation response
                // is not CORS-restricted.
                RequestMode::SameOrigin | RequestMode::Navigate => ResponseType::Basic,
            };

            // Store a non-redirected same-origin GET without `Set-Cookie`. A
            // `Vary` response is skipped: the cache key omits the request
            // headers, so honoring `Vary` would risk serving the wrong variant.
            if let Some(req_parts) = &cache_parts
                && !redirected
                && !parts.headers.contains_key(SET_COOKIE)
                && !parts.headers.contains_key(VARY)
                && let Some(res_parts) = decoded_response_parts(&parts)
            {
                lock_recovering(&self.cache).store(
                    self.partition,
                    req_parts,
                    &res_parts,
                    body_bytes.clone(),
                    now,
                );
            }

            // Decide what script may observe. An opaque (`no-cors`
            // cross-origin) response is blanked at the net boundary — status 0
            // and no headers, body kept for image decode — while a CORS
            // response exposes only its CORS-visible headers.
            let (status_code, status_text, response_headers) = match response_type {
                ResponseType::Opaque => (0, String::new(), Vec::new()),
                ResponseType::Cors => (
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("").to_owned(),
                    cors_visible_headers(&parts.headers, request.credentials),
                ),
                ResponseType::Basic => (
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("").to_owned(),
                    header_pairs(&parts.headers),
                ),
            };
            let head = ResponseHead {
                status: status_code,
                status_text,
                final_url: url.to_string(),
                headers: response_headers,
                redirected,
                response_type,
            };
            return Ok(FetchOutcome {
                head,
                body: body_bytes,
            });
        }
    }

    async fn load_file(&self, url: &Url) -> NetResult<FetchOutcome> {
        // `file::load_file` does blocking `std::fs` I/O; keep it off the async
        // worker so a slow disk can't stall the runtime.
        let policy = Arc::clone(&self.policy);
        let url_owned = url.clone();
        let f = tokio::task::spawn_blocking(move || file::load_file(&policy, &url_owned))
            .await
            .map_err(|e| {
                NetError::new(NetErrorKind::Io, format!("file load task failed: {e}"))
            })??;
        let mut headers = Vec::new();
        if let Some(ct) = f.content_type {
            headers.push(("content-type".to_owned(), ct));
        }
        Ok(FetchOutcome {
            head: ResponseHead {
                status: 200,
                status_text: "OK".to_owned(),
                final_url: url.to_string(),
                headers,
                redirected: false,
                response_type: ResponseType::Basic,
            },
            body: Bytes::from(f.bytes),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn build_request(
        &self,
        method: &Method,
        url: &Url,
        body: &[u8],
        user_headers: &[(String, String)],
        mode: RequestMode,
        referrer_source: Option<&Url>,
        origin: Option<&str>,
        send_cookies: bool,
        same_site: bool,
        now: SystemTime,
    ) -> NetResult<HttpRequest<Full<Bytes>>> {
        let uri: http::Uri = url
            .as_str()
            .parse()
            .map_err(|e| NetError::invalid_url(format!("{url}: {e}")))?;
        let mut builder = HttpRequest::builder().method(method.clone()).uri(uri);
        let headers = builder
            .headers_mut()
            .ok_or_else(|| NetError::invalid_url("failed to build request headers"))?;

        // In `no-cors` mode a request may carry only CORS-safelisted headers;
        // dropping the rest keeps `Authorization` and other sensitive script
        // headers off cross-origin no-cors loads (the Fetch no-cors safelist).
        let no_cors = mode == RequestMode::NoCors;
        for (name, value) in user_headers {
            let (name, value) = validate_header(name, value)?;
            if is_forbidden_header(&name) {
                continue;
            }
            if no_cors && !is_cors_safelisted_request_header(&name, &value) {
                continue;
            }
            headers.append(name, value);
        }
        headers.insert(
            ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, br, zstd, deflate"),
        );
        if !headers.contains_key(ACCEPT) {
            headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        }
        headers.insert(USER_AGENT, self.request_defaults.user_agent.clone());
        if !headers.contains_key(ACCEPT_LANGUAGE) {
            headers.insert(
                ACCEPT_LANGUAGE,
                self.request_defaults.accept_language.clone(),
            );
        }
        if let Some(source) = referrer_source
            && let Some(referer) = compute_referrer(source, url)
            && let Ok(value) = HeaderValue::from_str(&referer)
        {
            headers.insert(REFERER, value);
        }
        // `origin` is on the forbidden-header list, so script can never set or
        // override it; the value the caller computed is authoritative.
        if let Some(origin) = origin
            && let Ok(value) = HeaderValue::from_str(origin)
        {
            headers.insert(ORIGIN, value);
        }
        if send_cookies {
            let cookie = lock_recovering(&self.cookies).cookie_header(
                url,
                same_site,
                CookieSource::Http,
                now,
            );
            if let Some(cookie) = cookie
                && let Ok(value) = HeaderValue::from_str(&cookie)
            {
                headers.insert(COOKIE, value);
            }
        }

        let send_body = !matches!(*method, Method::GET | Method::HEAD);
        let body = if send_body {
            Full::new(Bytes::from(body.to_vec()))
        } else {
            Full::new(Bytes::new())
        };
        builder
            .body(body)
            .map_err(|e| NetError::invalid_url(format!("request build failed: {e}")))
    }

    async fn maybe_decompress(
        &self,
        headers: &HeaderMap,
        raw: Bytes,
        max: u64,
    ) -> NetResult<Bytes> {
        let encoding = headers
            .get(CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_ascii_lowercase());
        match encoding.as_deref() {
            Some(enc) if !enc.is_empty() && enc != "identity" => decompress(enc, raw, max).await,
            _ => Ok(raw),
        }
    }
}

/// Reads a body with a hard size cap (decompression/response bomb defense).
async fn read_body(body: Incoming, max: u64) -> NetResult<Bytes> {
    let limited = Limited::new(body, max as usize);
    match limited.collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) => Err(NetError::new(
            NetErrorKind::Decode,
            format!("response body read/limit exceeded: {e}"),
        )),
    }
}

/// A claim on the cumulative byte budget, held while a response body streams.
///
/// The full claim is charged to the counter up front. [`Self::commit`] marks how
/// much the response actually used; `Drop` refunds the rest, so an error, a
/// short body, or a cancelled read all leave the counter exact.
struct BudgetReservation {
    total: Arc<AtomicU64>,
    outstanding: u64,
}

impl BudgetReservation {
    /// The number of bytes this response may read.
    fn cap(&self) -> u64 {
        self.outstanding
    }

    /// Keeps `used` bytes charged; the remainder is refunded on drop.
    fn commit(&mut self, used: u64) {
        self.outstanding = self.outstanding.saturating_sub(used);
    }
}

impl Drop for BudgetReservation {
    fn drop(&mut self) {
        if self.outstanding > 0 {
            self.total.fetch_sub(self.outstanding, Ordering::AcqRel);
        }
    }
}

/// Decompresses a fully-buffered body, bounded by `max` (bomb defense).
async fn decompress(encoding: &str, body: Bytes, max: u64) -> NetResult<Bytes> {
    use async_compression::tokio::bufread as dec;

    let cursor = std::io::Cursor::new(body.clone());
    let mut out = Vec::new();
    let limit = max.saturating_add(1);
    let read = match encoding {
        "gzip" | "x-gzip" => {
            dec::GzipDecoder::new(cursor)
                .take(limit)
                .read_to_end(&mut out)
                .await
        }
        "br" => {
            dec::BrotliDecoder::new(cursor)
                .take(limit)
                .read_to_end(&mut out)
                .await
        }
        "zstd" => {
            dec::ZstdDecoder::new(cursor)
                .take(limit)
                .read_to_end(&mut out)
                .await
        }
        "deflate" => {
            dec::DeflateDecoder::new(cursor)
                .take(limit)
                .read_to_end(&mut out)
                .await
        }
        // An unknown or chained encoding (e.g. `gzip, br`) is a protocol error
        // — returning the still-compressed bytes as text is never correct.
        other => {
            return Err(NetError::new(
                NetErrorKind::Decode,
                format!("unsupported content-encoding `{other}`"),
            ));
        }
    };
    read.map_err(|e| NetError::new(NetErrorKind::Decode, format!("{encoding} decode: {e}")))?;
    if out.len() as u64 > max {
        return Err(NetError::new(
            NetErrorKind::Decode,
            "decompressed body exceeds size cap".to_owned(),
        ));
    }
    Ok(Bytes::from(out))
}

/// Wraps a decoded `data:` URL in the 200-response shape the rest of the stack
/// consumes, so a `data:` body is indistinguishable from a fetched one to every
/// caller — including the asynchronous ones, which keep their `NetEvent` timing.
///
/// The budget counters are deliberately not charged (matching `file://`, which
/// also returns above them): there is no request to rate-limit and no body to
/// stream, and `max_response_bytes` guards a decompression bomb arriving over
/// the wire, not bytes the caller already had in memory.
fn data_outcome(url: &Url) -> NetResult<FetchOutcome> {
    let body = data::load_data(url)?;
    Ok(FetchOutcome {
        head: ResponseHead {
            status: 200,
            status_text: "OK".to_owned(),
            final_url: url.to_string(),
            headers: vec![("content-type".to_owned(), body.content_type)],
            redirected: false,
            response_type: ResponseType::Basic,
        },
        body: Bytes::from(body.bytes),
    })
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

/// Applies the method/body downgrade rules for a redirect.
fn downgrade_method(status: StatusCode, method: &mut Method, body: &mut Vec<u8>) {
    match status {
        // 301/302: browsers downgrade POST → GET.
        StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND if *method == Method::POST => {
            *method = Method::GET;
            body.clear();
        }
        // 303: any non-GET/HEAD → GET.
        StatusCode::SEE_OTHER if *method != Method::GET && *method != Method::HEAD => {
            *method = Method::GET;
            body.clear();
        }
        // 307/308 (and same-method 301/302/303): preserve method and body.
        _ => {}
    }
}

/// Origin comparison (scheme + host + port). No initiator ⇒ same-origin.
fn is_cross_origin(initiator: Option<&Url>, target: &Url) -> bool {
    match initiator {
        Some(init) => init.origin() != target.origin(),
        None => false,
    }
}

/// Locks a mutex, recovering from poisoning.
///
/// A panic while the cookie jar or HTTP cache is held would otherwise poison the
/// lock and turn every later request on the page into a panic of its own. The
/// data behind these locks is a plain cache/jar with no cross-field invariant a
/// partial write could break, so continuing with it is strictly better than
/// bricking the page's network stack.
fn lock_recovering<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The `Origin` header value for a request, or `None` when Fetch does not
/// require one.
///
/// Fetch appends `Origin` to every request whose method is not `GET`/`HEAD`, and
/// to every cors-mode request. Omitting it (as this engine used to) leaves a
/// third-party server unable to see that a cross-origin `POST` was cross-origin
/// at all, defeating `Origin`-based CSRF checks, and breaks any CORS server that
/// requires the header. A request with no initiator (a top-level navigation)
/// has no origin to name.
fn request_origin(
    method: &Method,
    mode: RequestMode,
    initiator: Option<&Url>,
    opaque: bool,
) -> Option<String> {
    let needs_origin = !matches!(*method, Method::GET | Method::HEAD) || mode == RequestMode::Cors;
    if !needs_origin {
        return None;
    }
    if opaque {
        return Some("null".to_owned());
    }
    // An opaque initiator (`data:`, `about:`) serializes to "null" already.
    initiator.map(|url| url.origin().ascii_serialization())
}

/// Schemeful same-site: same registrable domain and same scheme. No
/// initiator (top-level navigation) ⇒ same-site.
fn is_same_site(initiator: Option<&Url>, target: &Url) -> bool {
    let Some(init) = initiator else {
        return true;
    };
    if init.scheme() != target.scheme() {
        return false;
    }
    match (init.host_str(), target.host_str()) {
        (Some(a), Some(b)) => match (registrable_domain(a), registrable_domain(b)) {
            (Some(ra), Some(rb)) => ra == rb,
            _ => a == b,
        },
        _ => false,
    }
}

/// Whether a request is CORS-"simple" (no preflight needed).
fn is_simple_request(method: &Method, headers: &[(String, String)]) -> bool {
    if !matches!(*method, Method::GET | Method::HEAD | Method::POST) {
        return false;
    }
    headers.iter().all(|(name, value)| {
        let n = name.to_ascii_lowercase();
        match n.as_str() {
            "accept" | "accept-language" | "content-language" => true,
            "content-type" => {
                let v = value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                matches!(
                    v.as_str(),
                    "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
                )
            }
            _ => false,
        }
    })
}

/// Validates a CORS response against the initiator origin and credentials.
fn check_cors(headers: &HeaderMap, initiator: Option<&Url>, creds: Credentials) -> NetResult<()> {
    let allow = headers
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| NetError::blocked("CORS: missing Access-Control-Allow-Origin"))?;
    if allow == "*" {
        if creds == Credentials::Include {
            return Err(NetError::blocked(
                "CORS: wildcard origin forbidden with credentials",
            ));
        }
        return Ok(());
    }
    let origin = initiator.map(|u| u.origin().ascii_serialization());
    if origin.as_deref() == Some(allow) {
        if creds == Credentials::Include {
            let acac = headers
                .get("access-control-allow-credentials")
                .and_then(|v| v.to_str().ok());
            if acac != Some("true") {
                return Err(NetError::blocked("CORS: credentials not allowed by server"));
            }
        }
        return Ok(());
    }
    Err(NetError::blocked(format!(
        "CORS: origin not permitted (Access-Control-Allow-Origin: {allow})"
    )))
}

/// `strict-origin-when-cross-origin` referrer, sanitized (no userinfo/
/// fragment; HTTP(S) only; no HTTPS→HTTP downgrade leak).
fn compute_referrer(source: &Url, target: &Url) -> Option<String> {
    if source.scheme() != "http" && source.scheme() != "https" {
        return None;
    }
    let mut sanitized = source.clone();
    let _ = sanitized.set_username("");
    let _ = sanitized.set_password(None);
    sanitized.set_fragment(None);

    if sanitized.origin() == target.origin() {
        return Some(sanitized.as_str().to_owned());
    }
    // Cross-origin: an HTTPS→HTTP downgrade leaks nothing.
    if source.scheme() == "https" && target.scheme() == "http" {
        return None;
    }
    let origin = sanitized.origin();
    if origin.is_tuple() {
        Some(format!("{}/", origin.ascii_serialization()))
    } else {
        None
    }
}

/// Rejects a header carrying CR/LF/NUL in the name or value (§8 hygiene).
fn validate_header(name: &str, value: &str) -> NetResult<(HeaderName, HeaderValue)> {
    let bad = |b: u8| b == b'\r' || b == b'\n' || b == 0;
    if name.bytes().any(bad) || value.bytes().any(bad) {
        return Err(NetError::blocked(format!(
            "illegal control character in header `{name}`"
        )));
    }
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| NetError::blocked(format!("invalid header name `{name}`")))?;
    let value = HeaderValue::from_str(value)
        .map_err(|_| NetError::blocked(format!("invalid value for header `{name}`")))?;
    Ok((name, value))
}

/// Whether a script-supplied request header is forbidden (Fetch §"forbidden
/// request-header name"): the client owns it, so a script copy is dropped.
fn is_forbidden_header(name: &HeaderName) -> bool {
    is_forbidden_request_header(name.as_str())
}

/// Whether a script-supplied request header is CORS-safelisted (Fetch): the
/// only headers a `no-cors` request may carry. Value constraints are applied
/// conservatively — an over-long or non-matching value drops the header rather
/// than smuggling it onto a cross-origin no-cors load.
fn is_cors_safelisted_request_header(name: &HeaderName, value: &HeaderValue) -> bool {
    // A safelisted value is at most 128 bytes (Fetch "CORS-safelisted
    // request-header").
    if value.as_bytes().len() > 128 {
        return false;
    }
    match name.as_str() {
        "accept" | "accept-language" | "content-language" => true,
        "content-type" => {
            let mime = value
                .to_str()
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            matches!(
                mime.as_str(),
                "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
            )
        }
        "range" => value.as_bytes().starts_with(b"bytes="),
        _ => false,
    }
}

/// Removes credential-bearing headers from the working set after a cross-origin
/// redirect so they are never replayed to the new origin (Fetch: strip
/// `Authorization` / `Proxy-Authorization` when the origin changes).
fn strip_cross_origin_credentials(headers: &mut Vec<(String, String)>) {
    headers.retain(|(name, _)| {
        let n = name.to_ascii_lowercase();
        n != "authorization" && n != "proxy-authorization"
    });
}

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_owned(), v.to_owned()))
        })
        .collect()
}

/// Builds the request `Parts` used as the HTTP cache key/policy input for a
/// plain GET (stable `Accept`/`Accept-Encoding`, no cookies).
fn cache_request_parts(url: &Url) -> NetResult<http::request::Parts> {
    let uri: http::Uri = url
        .as_str()
        .parse()
        .map_err(|e| NetError::invalid_url(format!("{url}: {e}")))?;
    let mut req = HttpRequest::builder()
        .method(Method::GET)
        .uri(uri)
        .body(())
        .map_err(|e| NetError::invalid_url(format!("cache key build failed: {e}")))?;
    let headers = req.headers_mut();
    headers.insert(
        ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, br, zstd, deflate"),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    Ok(req.into_parts().0)
}

/// Rebuilds response `Parts` for cache storage with the body already
/// decompressed: `Content-Encoding`/`Content-Length` are dropped so a served
/// hit is self-consistent (the stored body is plaintext).
fn decoded_response_parts(parts: &http::response::Parts) -> Option<http::response::Parts> {
    let mut builder = http::Response::builder()
        .status(parts.status)
        .version(parts.version);
    for (name, value) in &parts.headers {
        let n = name.as_str();
        if n == "content-encoding" || n == "content-length" {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }
    Some(builder.body(()).ok()?.into_parts().0)
}

/// Reconstructs a [`FetchOutcome`] from a cache hit (served as a
/// non-redirected response at the requested URL).
fn cached_outcome(cached: crate::cache::CachedResponse, url: &Url) -> FetchOutcome {
    FetchOutcome {
        head: ResponseHead {
            status: cached.status.as_u16(),
            status_text: cached.status.canonical_reason().unwrap_or("").to_owned(),
            final_url: url.to_string(),
            headers: header_pairs(&cached.headers),
            redirected: false,
            response_type: ResponseType::Basic,
        },
        body: cached.body,
    }
}

/// The response headers a CORS response exposes to script: the CORS-safelisted
/// set plus any named in `Access-Control-Expose-Headers` (or all non-cookie
/// headers when it is `*` and the request is not credentialed).
fn cors_visible_headers(headers: &HeaderMap, creds: Credentials) -> Vec<(String, String)> {
    const SAFELIST: &[&str] = &[
        "cache-control",
        "content-language",
        "content-length",
        "content-type",
        "expires",
        "last-modified",
        "pragma",
    ];
    let exposed: Vec<String> = headers
        .get("access-control-expose-headers")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let wildcard = creds != Credentials::Include && exposed.iter().any(|h| h == "*");
    header_pairs(headers)
        .into_iter()
        .filter(|(name, _)| {
            let n = name.to_ascii_lowercase();
            if n == "set-cookie" || n == "set-cookie2" {
                return false;
            }
            wildcard || SAFELIST.contains(&n.as_str()) || exposed.contains(&n)
        })
        .collect()
}

/// The `charset` parameter of a `Content-Type`, if it carries one.
///
/// Shared with `XMLHttpRequest`, whose "final charset" prefers the charset of
/// an `overrideMimeType()` value over the response's own and so has to ask this
/// question about each separately — one parsing rule, two callers.
#[must_use]
pub fn charset_from_content_type(content_type: &str) -> Option<&str> {
    content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("charset="))
        .map(|c| c.trim().trim_matches('"'))
        .filter(|c| !c.is_empty())
}

/// Decodes bytes to a string per a `Content-Type` charset (used by document
/// loads; Fetch `text()` always uses UTF-8 and does not call this).
#[must_use]
pub fn decode_charset(bytes: &[u8], content_type: Option<&str>) -> String {
    let label = content_type
        .and_then(charset_from_content_type)
        .unwrap_or("utf-8");
    decode_with_charset(bytes, label)
}

/// Decodes bytes with a named encoding, falling back to UTF-8 for an unknown
/// label — the Encoding standard's "get an encoding" plus its default.
#[must_use]
pub fn decode_with_charset(bytes: &[u8], label: &str) -> String {
    let encoding = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

/// Whether a script-supplied request header name is on Fetch's **forbidden
/// request-header** list — the names only the user agent may set.
///
/// `XMLHttpRequest.setRequestHeader` must silently ignore these, so the list
/// lives here next to the net layer that also strips them.
#[must_use]
pub fn is_forbidden_request_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name.starts_with("proxy-") || name.starts_with("sec-") {
        return true;
    }
    matches!(
        name.as_str(),
        "accept-charset"
            | "accept-encoding"
            | "access-control-request-headers"
            | "access-control-request-method"
            | "connection"
            | "content-length"
            | "cookie"
            | "cookie2"
            | "date"
            | "dnt"
            | "expect"
            | "host"
            | "keep-alive"
            | "origin"
            | "permissions-policy"
            | "referer"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "user-agent"
            | "via"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn referrer_same_origin_is_full_url() {
        let r = compute_referrer(
            &u("https://a.test/page?x=1#frag"),
            &u("https://a.test/other"),
        );
        assert_eq!(r.as_deref(), Some("https://a.test/page?x=1"));
    }

    #[test]
    fn forbidden_headers_cover_the_fetch_list() {
        let forbidden =
            |n: &str| is_forbidden_header(&HeaderName::from_bytes(n.as_bytes()).unwrap());
        // Newly-covered names and prefixes.
        assert!(forbidden("te"));
        assert!(forbidden("trailer"));
        assert!(forbidden("via"));
        assert!(forbidden("date"));
        assert!(forbidden("expect"));
        assert!(forbidden("dnt"));
        assert!(forbidden("proxy-authorization"));
        assert!(forbidden("sec-fetch-site"));
        // Still-allowed script headers.
        assert!(!forbidden("content-type"));
        assert!(!forbidden("x-custom"));
        assert!(!forbidden("authorization"));
    }

    #[test]
    fn cors_headers_pruned_to_safelist_and_exposed() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("text/plain"));
        headers.insert("x-secret", HeaderValue::from_static("hidden"));
        headers.insert("x-public", HeaderValue::from_static("shown"));
        headers.insert(
            "access-control-expose-headers",
            HeaderValue::from_static("x-public"),
        );
        let visible = cors_visible_headers(&headers, Credentials::SameOrigin);
        let names: Vec<&str> = visible.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"content-type")); // safelisted
        assert!(names.contains(&"x-public")); // explicitly exposed
        assert!(!names.contains(&"x-secret")); // pruned
    }

    #[test]
    fn referrer_cross_origin_is_origin_only() {
        let r = compute_referrer(&u("https://a.test/secret/path"), &u("https://b.test/"));
        assert_eq!(r.as_deref(), Some("https://a.test/"));
    }

    #[test]
    fn referrer_https_to_http_is_none() {
        let r = compute_referrer(&u("https://a.test/p"), &u("http://b.test/"));
        assert_eq!(r, None);
    }

    #[test]
    fn referrer_strips_userinfo() {
        let r = compute_referrer(&u("https://user:pass@a.test/p"), &u("https://a.test/q"));
        assert_eq!(r.as_deref(), Some("https://a.test/p"));
    }

    #[test]
    fn cross_origin_and_same_site() {
        assert!(!is_cross_origin(None, &u("https://a.test/")));
        assert!(is_cross_origin(
            Some(&u("https://a.test")),
            &u("https://b.test/")
        ));
        assert!(is_same_site(
            Some(&u("https://a.test")),
            &u("https://sub.a.test/")
        ));
        assert!(!is_same_site(
            Some(&u("https://a.test")),
            &u("https://b.test/")
        ));
        // Schemeful: http vs https is cross-site.
        assert!(!is_same_site(
            Some(&u("http://a.test")),
            &u("https://a.test/")
        ));
    }

    #[test]
    fn simple_request_detection() {
        assert!(is_simple_request(&Method::GET, &[]));
        assert!(is_simple_request(
            &Method::POST,
            &[("Content-Type".into(), "text/plain".into())]
        ));
        assert!(!is_simple_request(
            &Method::POST,
            &[("Content-Type".into(), "application/json".into())]
        ));
        assert!(!is_simple_request(&Method::PUT, &[]));
        assert!(!is_simple_request(
            &Method::GET,
            &[("X-Custom".into(), "1".into())]
        ));
    }

    #[test]
    fn header_validation_rejects_crlf() {
        assert!(validate_header("X-Test", "ok").is_ok());
        assert!(validate_header("X-Test", "bad\r\nInjected: 1").is_err());
        assert!(validate_header("X\nBad", "v").is_err());
    }

    #[test]
    fn charset_decode_utf8_default() {
        assert_eq!(decode_charset(b"h\xc3\xa9llo", None), "héllo");
        assert_eq!(
            decode_charset(&[0xe9], Some("text/html; charset=latin1")),
            "é"
        );
    }

    #[test]
    fn no_cors_safelist_drops_non_safelisted_headers() {
        let hn = |n: &str| HeaderName::from_bytes(n.as_bytes()).unwrap();
        let hv = |v: &str| HeaderValue::from_str(v).unwrap();
        // Authorization is a script header but not CORS-safelisted → dropped.
        assert!(!is_cors_safelisted_request_header(
            &hn("authorization"),
            &hv("Bearer x")
        ));
        assert!(!is_cors_safelisted_request_header(
            &hn("x-custom"),
            &hv("1")
        ));
        // Safelisted names pass; content-type only for the three simple MIMEs.
        assert!(is_cors_safelisted_request_header(&hn("accept"), &hv("*/*")));
        assert!(is_cors_safelisted_request_header(
            &hn("content-type"),
            &hv("text/plain")
        ));
        assert!(!is_cors_safelisted_request_header(
            &hn("content-type"),
            &hv("application/json")
        ));
        // Over-long values are not safelisted.
        assert!(!is_cors_safelisted_request_header(
            &hn("accept"),
            &hv(&"a".repeat(200))
        ));
    }

    #[test]
    fn cross_origin_redirect_strips_credentials() {
        let mut headers = vec![
            ("Authorization".to_owned(), "Bearer secret".to_owned()),
            ("X-Keep".to_owned(), "1".to_owned()),
            ("Proxy-Authorization".to_owned(), "creds".to_owned()),
        ];
        strip_cross_origin_credentials(&mut headers);
        let names: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["X-Keep"]);
    }

    fn engine(max_total_bytes: u64, max_response_bytes: u64) -> FetchEngine {
        let policy = ResourcePolicy {
            max_total_bytes,
            max_response_bytes,
            ..ResourcePolicy::default()
        };
        FetchEngine::new(Arc::new(policy), Arc::new(Mutex::new(CookieJar::new())))
            .expect("engine builds")
    }

    /// The core of the concurrency fix: a reservation removes its allowance from
    /// the shared counter immediately, so a second in-flight response sees the
    /// reduced headroom instead of the full budget. Sizing the read from an
    /// unreserved `max_total_bytes - total_bytes` let every concurrent fetch
    /// buffer against the same headroom.
    #[test]
    fn reservations_partition_the_budget() {
        let eng = engine(5000, 4000);

        let r1 = eng.reserve_bytes(None).expect("first reservation");
        assert_eq!(r1.cap(), 4000, "capped by max_response_bytes");

        let r2 = eng.reserve_bytes(None).expect("second reservation");
        assert_eq!(r2.cap(), 1000, "only the unreserved remainder is available");

        assert!(
            eng.reserve_bytes(None).is_err(),
            "a third reservation finds the budget exhausted"
        );

        drop(r2);
        assert_eq!(eng.total_charged_bytes(), 4000, "unused bytes are refunded");
        drop(r1);
        assert_eq!(eng.total_charged_bytes(), 0);
    }

    #[test]
    fn a_committed_reservation_charges_only_what_was_used() {
        let eng = engine(5000, 4000);
        {
            let mut reservation = eng.reserve_bytes(None).expect("reservation");
            assert_eq!(eng.total_charged_bytes(), 4000, "claimed up front");
            reservation.commit(1500);
        }
        assert_eq!(
            eng.total_charged_bytes(),
            1500,
            "the committed body stays charged, the rest is refunded"
        );
    }

    #[test]
    fn known_small_responses_reserve_their_declared_size() {
        let eng = engine(5000, 4000);
        let reservations: Vec<_> = (0..5)
            .map(|_| eng.reserve_bytes(Some(1000)).expect("small reservation"))
            .collect();

        assert!(
            reservations
                .iter()
                .all(|reservation| reservation.cap() == 1000)
        );
        assert_eq!(eng.total_charged_bytes(), 5000);
        assert!(eng.reserve_bytes(Some(1)).is_err());
    }

    #[test]
    fn origin_header_follows_the_fetch_rules() {
        let doc = u("https://example.com/page");

        // Same-origin GET in no-cors mode: no Origin.
        assert_eq!(
            request_origin(&Method::GET, RequestMode::NoCors, Some(&doc), false),
            None
        );
        // Any non-GET/HEAD request names its origin.
        assert_eq!(
            request_origin(&Method::POST, RequestMode::NoCors, Some(&doc), false),
            Some("https://example.com".to_owned())
        );
        // Every cors-mode request names its origin, even a GET.
        assert_eq!(
            request_origin(&Method::GET, RequestMode::Cors, Some(&doc), false),
            Some("https://example.com".to_owned())
        );
        // After a cross-origin redirect a cors request continues opaquely.
        assert_eq!(
            request_origin(&Method::GET, RequestMode::Cors, Some(&doc), true),
            Some("null".to_owned())
        );
        // A top-level navigation has no initiator to name.
        assert_eq!(
            request_origin(&Method::POST, RequestMode::Navigate, None, false),
            None
        );
    }
}
