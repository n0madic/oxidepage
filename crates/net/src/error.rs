//! Network errors (design doc §5.1, §5.5).
//!
//! [`NetError`] is the rich, owned error the fetch stack produces. It carries
//! a structured [`NetErrorKind`] category (defined in `base` so `EngineError`
//! can wrap it without a net dependency) plus an owned human-readable detail
//! (URL, status, or cause). It converts into `EngineError::Net` at the crate
//! boundary via `From`.

use oxidepage_base::{EngineError, NetErrorKind};

/// A network-layer failure with a structured category and owned detail.
#[derive(Clone, Debug)]
pub struct NetError {
    pub kind: NetErrorKind,
    /// The offending URL, status, or cause. Owned so no borrowed state
    /// crosses the async/sync boundary.
    pub detail: String,
    /// Whether `detail` is already a complete wire-level error text.
    ///
    /// See [`NetError::wire`]. Not public: the only way to set it is that
    /// constructor, so a `net::ERR_…` string cannot acquire the flag by
    /// accident, nor an ordinary detail lose its category prefix.
    verbatim: bool,
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.verbatim {
            return f.write_str(&self.detail);
        }
        write!(f, "{}: {}", self.kind.as_str(), self.detail)
    }
}

impl std::error::Error for NetError {}

impl NetError {
    #[must_use]
    pub fn new(kind: NetErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            verbatim: false,
        }
    }

    /// A failure whose detail is already Chrome's own `net::ERR_…` text.
    ///
    /// Such a string reaches a driver through `Network.loadingFailed.errorText`
    /// and `Page.navigate.errorText`, where it is compared by **equality** —
    /// Puppeteer's `navigateFrame` tests
    /// `errorText === 'net::ERR_HTTP_RESPONSE_CODE_FAILURE'`, and
    /// `request.abort(errorCode)` round-trips the name it was given. Gluing
    /// this error's category in front of it (`blocked: net::ERR_ABORTED`) is
    /// exactly what such a test never matches, so a verbatim detail is
    /// displayed alone. The `kind` is still carried, for the engine's own
    /// structured handling.
    #[must_use]
    pub fn wire(kind: NetErrorKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            detail: text.into(),
            verbatim: true,
        }
    }

    /// A policy/SSRF rejection (disallowed scheme, blocked address, budget).
    #[must_use]
    pub fn blocked(detail: impl Into<String>) -> Self {
        Self::new(NetErrorKind::Blocked, detail)
    }

    #[must_use]
    pub fn invalid_url(detail: impl Into<String>) -> Self {
        Self::new(NetErrorKind::InvalidUrl, detail)
    }

    #[must_use]
    pub fn protocol(detail: impl Into<String>) -> Self {
        Self::new(NetErrorKind::Protocol, detail)
    }

    #[must_use]
    pub fn aborted() -> Self {
        Self::new(NetErrorKind::Aborted, "request aborted")
    }

    #[must_use]
    pub fn kind(&self) -> NetErrorKind {
        self.kind
    }
}

impl From<NetError> for EngineError {
    fn from(err: NetError) -> Self {
        EngineError::Net {
            kind: err.kind,
            detail: err.detail,
        }
    }
}

/// A [`Result`] specialized to [`NetError`].
pub type NetResult<T> = Result<T, NetError>;
