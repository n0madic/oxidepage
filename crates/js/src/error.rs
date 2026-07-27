//! JS boundary errors.
//!
//! [`JsError`] flows out of the engine (evaluation and calls that threw);
//! [`JsThrow`] flows into it (host callbacks raising exceptions that page
//! script can catch).

use std::fmt;
use std::fmt::Write as _;

use thiserror::Error;

use crate::value::JsValue;

/// One frame of a JS call stack.
///
/// Parsed out of the engine's backtrace text ([`parse_stack`]) rather than
/// left as an opaque blob: an embedder — and, later, CDP's
/// `Runtime.CallFrame` — needs the URL and position as data, and re-parsing a
/// rendered string downstream is how the two representations drift.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StackFrame {
    /// The function name the engine recorded (`<eval>`, `<anonymous>` and the
    /// like included), `None` when it recorded none.
    pub function: Option<String>,
    /// The script URL the frame is in (the `filename` the script was
    /// evaluated with).
    pub url: String,
    /// 1-based line within that script.
    pub line: u32,
    /// 1-based column, `0` when the engine omitted it.
    pub column: u32,
}

impl fmt::Display for StackFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.function {
            Some(name) => write!(f, "at {name} ({}:{}:{})", self.url, self.line, self.column),
            None => write!(f, "at {}:{}:{}", self.url, self.line, self.column),
        }
    }
}

/// Parses a QuickJS-NG backtrace into frames, innermost first.
///
/// The engine's format is one `    at <fn> (<url>:<line>:<column>)` per line.
/// Frames with no script position (`    at forEach (native)`) are dropped:
/// they carry nothing an embedder can act on, and they are not where the page
/// went wrong.
#[must_use]
pub fn parse_stack(stack: &str) -> Vec<StackFrame> {
    stack.lines().filter_map(parse_frame).collect()
}

/// The innermost frame with a script position, without parsing the rest.
///
/// The console needs exactly this, on every call: parsing a whole backtrace
/// to keep one frame is allocation nothing reads.
#[must_use]
pub fn parse_first_frame(stack: &str) -> Option<StackFrame> {
    stack.lines().find_map(parse_frame)
}

fn parse_frame(line: &str) -> Option<StackFrame> {
    let rest = line.trim().strip_prefix("at ")?;
    // `<fn> (<location>)`, or a bare `<location>` when the engine named no
    // function. Split on the *last* " (" so a function name containing one
    // does not break the frame apart in the wrong place.
    let (function, location) = match rest.rfind(" (") {
        Some(at) if rest.ends_with(')') => (Some(&rest[..at]), &rest[at + 2..rest.len() - 1]),
        _ => (None, rest),
    };
    let function = function
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned);
    // `native` — and anything else not ending in `:<number>` — is not a script
    // position.
    let (head, last) = location.rsplit_once(':')?;
    // Try `url:line:column` first. A URL's own colons (`http://`, a port) never
    // parse as a number, so the two-component fallback below is what catches an
    // engine that omitted the column rather than a mis-split URL.
    if let Ok(column) = last.parse::<u32>()
        && let Some((url, mid)) = head.rsplit_once(':')
        && let Ok(line) = mid.parse::<u32>()
    {
        return Some(StackFrame {
            function,
            url: url.to_owned(),
            line,
            column,
        });
    }
    Some(StackFrame {
        function,
        url: head.to_owned(),
        line: last.parse::<u32>().ok()?,
        column: 0,
    })
}

/// An error produced while evaluating or calling into a realm.
#[derive(Debug, Error)]
pub enum JsError {
    /// Script threw. Carries the exception's structure — constructor name,
    /// bare message, parsed stack — and the thrown value itself.
    ///
    /// `Display` is the **bare message**: the stack is data now, and a caller
    /// that wants the classic two-part rendering asks for
    /// [`JsError::rendered`].
    #[error("{message}")]
    Exception {
        /// The exception's `name` (`"TypeError"`), when it has one.
        name: Option<String>,
        message: String,
        stack: Vec<StackFrame>,
        value: Option<JsValue>,
    },
    /// Engine-level failure (allocation, unrelated-runtime, …) with no JS
    /// exception value.
    #[error("js engine failure: {0}")]
    Engine(String),
}

impl JsError {
    /// The message with the stack appended, one indented frame per line —
    /// what an uncaught exception looks like in a log.
    ///
    /// Also the identity an embedder has for an exception it never saw the
    /// value of: the page crate keys unhandled-rejection retraction on this,
    /// because the engine's rejection tracker carries no promise identity.
    #[must_use]
    pub fn rendered(&self) -> String {
        let JsError::Exception { message, stack, .. } = self else {
            return self.to_string();
        };
        let mut out = message.clone();
        for frame in stack {
            let _ = write!(out, "\n    {frame}");
        }
        out
    }

    /// The exception's `name`, when it is an exception that has one.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            JsError::Exception { name, .. } => name.as_deref(),
            JsError::Engine(_) => None,
        }
    }

    /// The parsed call stack (empty for engine failures and for exceptions
    /// thrown as non-`Error` values).
    #[must_use]
    pub fn stack(&self) -> &[StackFrame] {
        match self {
            JsError::Exception { stack, .. } => stack,
            JsError::Engine(_) => &[],
        }
    }
}

/// An exception raised by a host callback, surfaced to page script.
///
/// Spec-named `DOMException`s are constructed as real JS objects by the
/// bindings and thrown via [`JsThrow::Value`]; the engine crate only knows
/// the ECMAScript-native error kinds it must be able to mint itself.
#[derive(Debug)]
pub enum JsThrow {
    /// `TypeError` (the WebIDL workhorse).
    Type(String),
    /// `RangeError`.
    Range(String),
    /// Throw this exact value.
    Value(JsValue),
}

impl From<JsError> for JsThrow {
    /// Propagates an error from a nested engine call out of a host callback,
    /// rethrowing the original exception value when there is one.
    fn from(err: JsError) -> Self {
        match err {
            JsError::Exception {
                value: Some(value), ..
            } => JsThrow::Value(value),
            JsError::Exception { message, .. } => JsThrow::Type(message),
            JsError::Engine(message) => JsThrow::Type(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StackFrame, parse_stack};

    #[test]
    fn parses_frames_and_drops_native_ones() {
        let stack = "    at cb (http://x/t.js:1:55)\n    at forEach (native)\n    at <eval> (http://x/t.js:3:1)\n";
        assert_eq!(
            parse_stack(stack),
            vec![
                StackFrame {
                    function: Some("cb".into()),
                    url: "http://x/t.js".into(),
                    line: 1,
                    column: 55,
                },
                StackFrame {
                    function: Some("<eval>".into()),
                    url: "http://x/t.js".into(),
                    line: 3,
                    column: 1,
                },
            ]
        );
    }

    #[test]
    fn keeps_the_port_out_of_the_line_number() {
        let frames = parse_stack("    at f (http://127.0.0.1:8080/a.js:12:3)");
        assert_eq!(frames[0].url, "http://127.0.0.1:8080/a.js");
        assert_eq!((frames[0].line, frames[0].column), (12, 3));
    }

    #[test]
    fn falls_back_when_the_column_is_absent() {
        let frames = parse_stack("    at f (http://127.0.0.1:8080/a.js:12)");
        assert_eq!(frames[0].url, "http://127.0.0.1:8080/a.js");
        assert_eq!((frames[0].line, frames[0].column), (12, 0));
    }

    #[test]
    fn accepts_an_unnamed_frame() {
        let frames = parse_stack("    at file.js:2:4");
        assert_eq!(frames[0].function, None);
        assert_eq!(frames[0].url, "file.js");
    }

    #[test]
    fn ignores_lines_that_are_not_frames() {
        assert!(parse_stack("TypeError: boom\n\n   nonsense").is_empty());
    }
}
