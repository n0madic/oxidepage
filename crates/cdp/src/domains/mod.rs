//! Command handlers, one module per CDP domain.
//!
//! **The allow-list is the code.** There is no generated dispatch table and no
//! catch-all that guesses: a method reaches a handler only because someone wrote
//! a match arm for it, and everything else falls through to
//! [`ProtocolError::method_not_found`](crate::error::ProtocolError::method_not_found).
//! That is P6 ("absent beats fake", design §2) applied to the protocol — a
//! driver that is told `Emulation.setGeolocationOverride` does not exist can
//! report a clear failure, where a silent `{}` would leave a test asserting
//! against a location that was never set.
//!
//! Params are hand-written `serde` structs rather than generated from
//! `browser_protocol.json` (ADR-0030): the implemented subset is ~60 commands,
//! the protocol version is pinned, and a handler that disagrees with its params
//! is already a compile error — so a second code generator would carry the cost
//! of vendoring ~2.5 MB of JSON for drift protection the compiler already gives.

pub mod browser;
pub mod emulation;
pub mod fetch;
pub mod io;
pub mod log;
pub mod network;
pub mod page;
pub mod performance;
pub mod runtime;
pub mod target;
