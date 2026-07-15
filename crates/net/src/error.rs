//! Network errors (design doc §5.1, §5.5).
//!
//! [`NetError`] is the rich, owned error the fetch stack produces. It carries
//! a structured [`NetErrorKind`] category (defined in `base` so `EngineError`
//! can wrap it without a net dependency) plus an owned human-readable detail
//! (URL, status, or cause). It converts into `EngineError::Net` at the crate
//! boundary via `From`.

use oxidepage_base::{EngineError, NetErrorKind};

/// A network-layer failure with a structured category and owned detail.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{}: {detail}", kind.as_str())]
pub struct NetError {
    pub kind: NetErrorKind,
    /// The offending URL, status, or cause. Owned so no borrowed state
    /// crosses the async/sync boundary.
    pub detail: String,
}

impl NetError {
    #[must_use]
    pub fn new(kind: NetErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
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
