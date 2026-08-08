//! Protocol errors.
//!
//! CDP reuses JSON-RPC 2.0's error codes for malformed traffic and adds its own
//! `-32000` band for "the request was well-formed but the browser refused it".
//! Every method outside the allow-list answers [`ProtocolError::method_not_found`]
//! rather than a stub result — P6, "absent beats fake": a driver that is told a
//! method does not exist can fall back, one that receives a silent no-op cannot.

use serde::Serialize;

/// The JSON-RPC error object carried by a failed [`crate::message::Response`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

impl ProtocolError {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    /// The CDP-specific band: a valid request the browser declined to perform.
    pub const SERVER_ERROR: i32 = -32000;

    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    #[must_use]
    pub fn with_data(mut self, data: impl Into<String>) -> Self {
        self.data = Some(data.into());
        self
    }

    #[must_use]
    pub fn parse_error(detail: impl Into<String>) -> Self {
        Self::new(Self::PARSE_ERROR, "Message must be a valid JSON").with_data(detail)
    }

    #[must_use]
    pub fn invalid_request(detail: impl Into<String>) -> Self {
        Self::new(Self::INVALID_REQUEST, detail)
    }

    /// The uniform answer for anything outside the implemented allow-list.
    ///
    /// The wording matches Chrome's (`'X.y' wasn't found`) because drivers do
    /// match on it: Puppeteer's feature detection reads the message, not only
    /// the code.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(Self::METHOD_NOT_FOUND, format!("'{method}' wasn't found"))
    }

    #[must_use]
    pub fn invalid_params(detail: impl Into<String>) -> Self {
        Self::new(Self::INVALID_PARAMS, detail)
    }

    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(Self::INTERNAL_ERROR, detail)
    }

    /// A well-formed request the browser refused or could not complete.
    #[must_use]
    pub fn server(detail: impl Into<String>) -> Self {
        Self::new(Self::SERVER_ERROR, detail)
    }

    /// The target named by a `sessionId` is gone.
    #[must_use]
    pub fn no_session(session_id: &str) -> Self {
        Self::server(format!("Session with given id {session_id} not found."))
    }

    #[must_use]
    pub fn no_target(target_id: &str) -> Self {
        Self::server(format!("No target with given id {target_id}"))
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)?;
        if let Some(data) = &self.data {
            write!(f, " ({data})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ProtocolError {}

/// An engine failure, rendered into the protocol's vocabulary.
///
/// [`oxidepage_engine::EngineError`] distinguishes a closed page from a crashed
/// one; the protocol has no such distinction, so both become `-32000` with the
/// cause in the message rather than being flattened to a bare "failed".
impl From<oxidepage_engine::EngineError> for ProtocolError {
    fn from(error: oxidepage_engine::EngineError) -> Self {
        use oxidepage_engine::EngineError;
        match error {
            EngineError::Closed => ProtocolError::server("Target closed"),
            EngineError::Crashed(message) => {
                ProtocolError::server("Target crashed").with_data(message)
            }
            EngineError::Timeout => ProtocolError::server("Timed out waiting for the target"),
            EngineError::Launch(message) => ProtocolError::internal(message),
            EngineError::PopupBlocked => ProtocolError::server("Popup blocked"),
            other => ProtocolError::server(other.to_string()),
        }
    }
}

/// The result of dispatching one command.
pub type CommandResult = Result<serde_json::Value, ProtocolError>;

/// What dispatching one command produced: an answer, or a promise of one.
///
/// Only `Runtime.evaluate`, `Runtime.callFunctionOn` and
/// `Runtime.awaitPromise` can defer, and only when the driver asked for
/// `awaitPromise` on a promise that has not settled (ADR-0034 D1). Everything
/// else is `Ready`, which is why the whole dispatch tree keeps returning
/// [`CommandResult`] and only the Runtime domain speaks this type.
pub enum Deferrable {
    /// Reply now.
    Ready(CommandResult),
    /// Reply when [`PageEvent::AwaitSettled`](oxidepage_engine::PageEvent)
    /// carries this token. The lane sends **no** response — that is the point:
    /// the session's next command runs while the promise is still pending, and
    /// resolving it is very often exactly what that command does.
    Deferred(oxidepage_engine::page_api::AwaitToken),
}

impl From<CommandResult> for Deferrable {
    fn from(result: CommandResult) -> Self {
        Self::Ready(result)
    }
}

/// Failures of the transport itself, as opposed to of a command.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to start the server runtime: {0}")]
    Runtime(#[source] std::io::Error),
}
