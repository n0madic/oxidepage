//! Opaque identifiers, and the one source of randomness in the crate.
//!
//! The DevTools endpoint is total remote control of a process that executes
//! attacker-controlled content (`docs/automation-roadmap.md`, "Security note").
//! Binding to loopback keeps a remote host out; the path token stops a *blind*
//! attacker — one scanning ports without being able to make requests that are
//! read back — from reaching the protocol. That makes it a security boundary,
//! so it must come from a CSPRNG and not from a counter, a clock, or a
//! `DefaultHasher` seed.
//!
//! **What it does not do.** `/json/version` hands the token out, unauthenticated,
//! in `webSocketDebuggerUrl` — as Chrome's does, because
//! `puppeteer.connect({ browserURL })` discovers the socket that way. So the
//! token is not a secret from anything that can issue an HTTP request to the
//! port and read the reply. The defences that actually cover that case are the
//! loopback bind and the `Host` check in [`crate::http::host_is_local`], which
//! is what stops a hostile web page from rebinding a name to `127.0.0.1` and
//! walking in through the browser. Anyone who can already run code on this
//! machine and talk to loopback is inside the boundary, and no token changes
//! that.
//!
//! The generator is `rustls`'s ring provider. `rustls` is already a workspace
//! dependency and already linked into every binary through `oxidepage-net`, so
//! reaching for its `SecureRandom` costs nothing, whereas a `rand` entry in
//! `[workspace.dependencies]` would add a dependency tree for sixteen bytes.

/// 128 bits, hex-encoded — the shape Chrome uses for target and session ids.
const TOKEN_BYTES: usize = 16;

/// A fresh 32-character lowercase hex id.
///
/// # Panics
///
/// If the platform CSPRNG fails. There is no safe fallback: a predictable token
/// is a predictable capability URL, and continuing with a weak id would hand out
/// the process under the appearance of a random one. A `getrandom` failure means
/// the process is already in a state (no entropy source, exhausted descriptors)
/// where refusing to serve is the correct outcome.
#[must_use]
pub fn random_hex() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .expect("platform CSPRNG unavailable; refusing to mint a guessable DevTools token");

    let mut out = String::with_capacity(TOKEN_BYTES * 2);
    for byte in bytes {
        // `write!` to a `String` is infallible, but formatting machinery for a
        // hot-ish path is needless: two table lookups do the same job.
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Constant-time-ish comparison for the path token.
///
/// Timing on a loopback HTTP path compare is not a realistic oracle, but the
/// comparison is one line either way and a short-circuiting `==` on a secret is
/// the kind of thing a reader has to stop and reason about.
#[must_use]
pub fn token_matches(expected: &str, provided: &str) -> bool {
    if expected.len() != provided.len() {
        return false;
    }
    expected
        .bytes()
        .zip(provided.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_thirty_two_hex_characters() {
        let token = random_hex();
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(token.bytes().all(|b| !b.is_ascii_uppercase()));
    }

    #[test]
    fn tokens_do_not_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            assert!(seen.insert(random_hex()), "CSPRNG returned a duplicate");
        }
    }

    #[test]
    fn token_comparison_accepts_only_an_exact_match() {
        let token = random_hex();
        assert!(token_matches(&token, &token.clone()));
        assert!(!token_matches(&token, &token[..31]));
        assert!(!token_matches(&token, &format!("{token}x")));
        assert!(!token_matches(&token, &random_hex()));
        assert!(!token_matches(&token, &token.to_uppercase()));
    }
}
