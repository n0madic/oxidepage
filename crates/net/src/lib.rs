//! Fetch stack: HTTP(S), cookies, cache, policy, and the SSRF guard
//! (design doc §5.5, §8).
//!
//! The layering, from the wire up:
//! - [`ip_filter`] / [`policy`]: the address-safety predicate and the
//!   secure-default resource policy.
//! - [`connector`]: the single SSRF enforcement point — a `Service<Uri>`
//!   that resolves and vets every address before connecting.
//! - [`client`]: the connector wrapped in `hyper-rustls` TLS + a pooling
//!   hyper client.
//! - [`cookies`] / [`cache`]: an RFC 6265bis jar scoped to a browsing context
//!   and an RFC 9111 in-memory cache, partitioned per context.
//! - [`fetch`]: the hand-rolled redirect/cookie/referrer pipeline (re-enters
//!   per hop, so SSRF is re-validated on every redirect).
//! - [`file`]: opt-in, jailed `file://` loading.
//! - [`data`]: `data:` URL decoding, handled beside `file://` above the scheme
//!   gate so every consumer of the pipeline gets it without a special case.
//! - [`intercept`]: the pause point's vocabulary — patterns, decisions, and the
//!   config a driver thread and a page thread share (ADR-0032).
//! - [`service`]: `NetService`, the async net ↔ sync page bridge, plus
//!   `NetPool` — the runtime, connection pool and cache a browser shares
//!   across its pages (ADR-0027 D7).

pub mod cache;
pub mod client;
pub mod connector;
pub mod cookies;
pub mod data;
pub mod error;
pub mod fetch;
pub mod file;
pub mod intercept;
pub mod ip_filter;
pub mod policy;
pub mod record;
pub mod service;

pub use cache::{CachePartition, HttpCache};
pub use client::HttpClient;
pub use cookies::{CookieJar, CookieSource, CookieView};
pub use data::DataBody;
pub use error::{NetError, NetResult};
pub use fetch::{
    ContentDisposition, Credentials, FetchEngine, FetchOutcome, NetRequest, RequestDefaults,
    RequestMode, ResourceType, ResponseHead, ResponseType, SharedFetchParts, basic_auth_header,
    charset_from_content_type, decode_charset, decode_with_charset, is_forbidden_request_header,
    parse_auth_challenge, parse_content_disposition, sanitize_filename,
};
pub use intercept::{
    AuthChallenge, AuthResponse, AuthSource, DEFAULT_INTERCEPT_TIMEOUT, FulfilledResponse,
    InterceptCommand, InterceptConfig, InterceptControl, RequestOverrides, RequestPattern,
};
pub use ip_filter::ip_allowed;
pub use policy::ResourcePolicy;
pub use record::{NetworkEvent, RequestLog};
pub use service::{NetEvent, NetPool, NetPoolOptions, NetService, SharedNetConfig};

pub use oxidepage_base::{NetErrorKind, RequestId};
