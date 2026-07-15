//! Re-exports of the interned string atoms shared with html5ever and stylo.
//!
//! All crates name DOM identifiers through these aliases so the whole engine
//! agrees on one interning table (design doc §3.2, `string_cache`/`web_atoms`).

pub use web_atoms::{
    LocalName, Namespace, Prefix, local_name, namespace_prefix, namespace_url, ns,
};
