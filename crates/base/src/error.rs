//! The engine error hierarchy.
//!
//! Structured errors only — no stringly-typed errors cross crate boundaries
//! (design doc §5.1). Domain crates add variants to [`EngineError`] as their
//! phases land.

use thiserror::Error;

/// Spec-defined `DOMException` names the engine can raise.
///
/// Lives in `base` because both the DOM implementation (which raises them)
/// and the JS bindings (which surface them to script as `DOMException`
/// objects) need the same vocabulary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum DomExceptionKind {
    /// The operation would yield an incorrect node tree.
    HierarchyRequestError,
    /// The object is in an invalid state.
    InvalidStateError,
    /// The string contains invalid characters.
    InvalidCharacterError,
    /// The object can not be found here.
    NotFoundError,
    /// The operation is not supported.
    NotSupportedError,
    /// The object is in use (e.g. an attribute already owned by an element).
    InUseAttributeError,
    /// The supplied node is incorrect or has an incorrect ancestor.
    InvalidNodeTypeError,
    /// A generic syntax error (selectors, etc.).
    SyntaxError,
    /// The index is not in the allowed range.
    IndexSizeError,
    /// The object can not be modified (e.g. `outerHTML` without a parent).
    NoModificationAllowedError,
    /// The operation is not allowed by namespace rules.
    NamespaceError,
    /// The request is not allowed by the user agent or the platform (e.g.
    /// `replaceSync` on a non-constructed stylesheet).
    NotAllowedError,
}

impl DomExceptionKind {
    /// The spec-defined exception name, as page script would observe it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::HierarchyRequestError => "HierarchyRequestError",
            Self::InvalidStateError => "InvalidStateError",
            Self::InvalidCharacterError => "InvalidCharacterError",
            Self::NotFoundError => "NotFoundError",
            Self::NotSupportedError => "NotSupportedError",
            Self::InUseAttributeError => "InUseAttributeError",
            Self::InvalidNodeTypeError => "InvalidNodeTypeError",
            Self::SyntaxError => "SyntaxError",
            Self::IndexSizeError => "IndexSizeError",
            Self::NoModificationAllowedError => "NoModificationAllowedError",
            Self::NamespaceError => "NamespaceError",
            Self::NotAllowedError => "NotAllowedError",
        }
    }
}

/// A raised `DOMException`: a spec name plus a static context message.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Error)]
#[error("{}: {message}", kind.name())]
pub struct DomException {
    pub kind: DomExceptionKind,
    /// Human-oriented context; never inspected programmatically.
    pub message: &'static str,
}

impl DomException {
    #[must_use]
    pub fn new(kind: DomExceptionKind, message: &'static str) -> Self {
        Self { kind, message }
    }
}

/// A category of network-layer failure.
///
/// Lives in `base` so [`EngineError`] can carry a network failure without
/// `base` depending on `net`: the `net` crate owns the rich `NetError` type
/// and converts it to `EngineError::Net { kind, detail }` at the boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum NetErrorKind {
    /// The request violated the resource policy (disallowed scheme, budget
    /// exceeded, or an SSRF-blocked address).
    Blocked,
    /// The URL was malformed or unsupported.
    InvalidUrl,
    /// DNS resolution failed or yielded no usable address.
    Dns,
    /// The TCP connection could not be established.
    Connect,
    /// The TLS handshake failed.
    Tls,
    /// The request exceeded its time budget.
    Timeout,
    /// A malformed HTTP response, or a non-2xx status surfaced as an error.
    Protocol,
    /// The redirect chain exceeded the policy cap.
    TooManyRedirects,
    /// Response/content decoding (charset or compression) failed.
    Decode,
    /// A `file://` load failed (missing, jail escape, non-regular file).
    File,
    /// The request was cancelled by the page.
    Aborted,
    /// A lower-level I/O failure with no more specific category.
    Io,
}

impl NetErrorKind {
    /// A short, stable slug for the category.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::InvalidUrl => "invalid-url",
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Timeout => "timeout",
            Self::Protocol => "protocol",
            Self::TooManyRedirects => "too-many-redirects",
            Self::Decode => "decode",
            Self::File => "file",
            Self::Aborted => "aborted",
            Self::Io => "io",
        }
    }
}

/// Root of the engine error hierarchy.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EngineError {
    /// A DOM operation failed with a spec-defined exception.
    #[error(transparent)]
    Dom(#[from] DomException),
    /// An id failed its generation check: the entity it referred to is gone.
    #[error("stale id: {0}")]
    StaleId(&'static str),
    /// A network request failed. The `detail` carries the offending URL,
    /// status, or cause as an owned string (structured category, human
    /// message — no stringly-typed matching across crates).
    #[error("network error ({}): {detail}", kind.as_str())]
    Net { kind: NetErrorKind, detail: String },
}
