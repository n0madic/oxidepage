//! The structured payloads a page produces for its embedder: console lines
//! and script errors.
//!
//! Both live here rather than in `page` because the bindings are what *build*
//! them — the call site is the only place with a JS scope to read argument
//! values and capture a stack from. `page` re-exports them, so an embedder has
//! one import path (ADR-0025 D9).
//!
//! Nothing in this module holds a `JsValue`. These payloads sit in streams the
//! embedder drains at its leisure, outliving the navigation that produced
//! them, and a `JsObject` "must be dropped before its realm is torn down"
//! (`oxidepage_js::JsObject`).

use std::fmt;

use oxidepage_js::{JsError, StackFrame};

use crate::preview::ValuePreview;

/// Console message severity, mirroring the console API methods.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConsoleLevel {
    Log,
    Info,
    Warn,
    Error,
    Debug,
    Trace,
}

impl ConsoleLevel {
    /// The console **method name** this level came from, and what the CLI
    /// prints.
    ///
    /// Close to, but *not*, CDP's `Runtime.consoleAPICalled.type`: that enum
    /// spells `console.warn`'s level `warning`. The protocol layer maps it
    /// (`crates/cdp/src/pump.rs`); this stays the method name, which is what a
    /// human reading CLI output expects to see.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ConsoleLevel::Log => "log",
            ConsoleLevel::Info => "info",
            ConsoleLevel::Warn => "warn",
            ConsoleLevel::Error => "error",
            ConsoleLevel::Debug => "debug",
            ConsoleLevel::Trace => "trace",
        }
    }
}

/// A console line captured from page script.
#[derive(Clone, Debug, PartialEq)]
pub struct ConsoleMessage {
    pub level: ConsoleLevel,
    /// The rendered line: format specifiers applied, values rendered.
    pub message: String,
    /// One bounded preview per argument, *before* formatting. CDP's
    /// `consoleAPICalled` sends the raw arguments and lets the client decide
    /// how to show them, and so do we — [`ConsoleMessage::message`] is our
    /// answer, not the only possible one.
    ///
    /// Empty for an engine-originated message.
    pub args: Vec<ValuePreview>,
    /// The innermost script frame at the call site. `None` for
    /// engine-originated messages and for a call with no script on the stack.
    pub location: Option<StackFrame>,
    /// `console.group` nesting depth at the time of the call. Grouping has no
    /// other observable effect headless, which is why the depth is carried
    /// rather than the group structure.
    pub group_depth: u32,
    /// Unix-epoch milliseconds, from the page's monotonic time origin (the
    /// same clock as `NavigationEvent::timestamp`).
    pub timestamp: f64,
}

impl ConsoleMessage {
    /// An engine-originated message: no JS arguments, no call site.
    ///
    /// This is the P6 "the API is present but this effect is out of reach"
    /// announcement path (`BindCx::warn`), not something page script called.
    #[must_use]
    pub fn engine(level: ConsoleLevel, message: String, timestamp: f64) -> Self {
        Self {
            level,
            message,
            args: Vec::new(),
            location: None,
            group_depth: 0,
            timestamp,
        }
    }
}

/// Where a script error came from — the discriminant a driver routes on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScriptErrorKind {
    /// An uncaught exception in a classic script, a module, or an event
    /// handler attribute.
    Uncaught,
    /// A callback the engine invoked threw, and control flow continued (spec
    /// "report the exception"): a listener, an observer, a timer, a frame
    /// callback.
    Callback,
    /// A promise rejected and nothing ever handled it.
    UnhandledRejection,
    /// The task outran its `ScriptBudget` and was interrupted.
    ScriptBudget,
    /// A subresource or the engine failed, with no JS exception involved: a
    /// stylesheet that 404'd, a module specifier that would not resolve, a
    /// web font the shaper rejected.
    ///
    /// Its own kind rather than folded into [`ScriptErrorKind::Uncaught`],
    /// because a driver routes it differently (CDP's `Log.entryAdded`, not
    /// `Runtime.exceptionThrown`) — and because a `kind` that lumps unlike
    /// things together is a `kind` nobody can act on.
    Resource,
}

impl ScriptErrorKind {
    fn prefix(self) -> Option<&'static str> {
        match self {
            ScriptErrorKind::UnhandledRejection => Some("unhandled promise rejection"),
            _ => None,
        }
    }
}

/// An error the *page* produced — not an error of the engine API.
///
/// The plain-data projection of [`JsError`], which carries the thrown
/// `JsValue` and therefore must not enter a drained stream.
#[derive(Clone, PartialEq, Debug)]
pub struct ScriptError {
    pub kind: ScriptErrorKind,
    /// The exception's `name` (`"TypeError"`), absent when the thrown value
    /// was not an `Error`.
    pub name: Option<String>,
    /// The bare message, with no stack glued on.
    pub message: String,
    /// Innermost frame first; empty when the engine recorded none.
    pub stack: Vec<StackFrame>,
    /// Unix-epoch milliseconds, from the page's monotonic time origin.
    pub timestamp: f64,
}

impl ScriptError {
    /// Structures an engine error. `kind` is the caller's knowledge — the
    /// engine cannot tell an uncaught script exception from a listener that
    /// threw.
    #[must_use]
    pub fn from_js(kind: ScriptErrorKind, error: &JsError, timestamp: f64) -> Self {
        Self {
            kind,
            name: error.name().map(ToOwned::to_owned),
            message: error.to_string(),
            stack: error.stack().to_vec(),
            timestamp,
        }
    }

    /// An error the engine itself reports, with no JS exception behind it
    /// (a failed observer entry, an aborted script).
    #[must_use]
    pub fn engine(kind: ScriptErrorKind, message: String, timestamp: f64) -> Self {
        Self {
            kind,
            name: None,
            message,
            stack: Vec::new(),
            timestamp,
        }
    }

    /// The throw site: the innermost script frame, when there is one.
    ///
    /// Not a field, because a field and `stack[0]` are two representations of
    /// one fact and would eventually disagree.
    #[must_use]
    pub fn location(&self) -> Option<&StackFrame> {
        self.stack.first()
    }
}

impl fmt::Display for ScriptError {
    /// One line: the kind prefix (only unhandled rejections have one),
    /// the error name, and the message. The stack is deliberately *not*
    /// rendered here — a caller that wants it walks [`ScriptError::stack`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(prefix) = self.kind.prefix() {
            write!(f, "{prefix}: ")?;
        }
        match &self.name {
            Some(name) => write!(f, "{name}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}
