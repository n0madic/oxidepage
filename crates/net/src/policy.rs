//! Resource policy (design doc §5.5): the secure-by-default gate every load
//! is checked against. The scheme allowlist is pinned across redirects, the
//! SSRF filter defaults on, `file://` defaults off, and byte/count/redirect
//! budgets are enforced by the fetch pipeline.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::ip_filter::ip_allowed;

/// Per-page network policy. Shared (`Arc`) by the connector, the fetch
/// pipeline, and document/subresource loads so the same gate applies
/// everywhere.
#[derive(Clone, Debug)]
pub struct ResourcePolicy {
    /// Schemes a load may use, checked on the initial request and re-checked
    /// on every redirect hop. Lowercase.
    pub allowed_schemes: Vec<String>,
    /// Apply the SSRF address filter ([`ip_allowed`]). On by default.
    pub block_private_hosts: bool,
    /// Permit `file://` loads at all. Off by default; network-origin
    /// documents must never turn this on.
    pub allow_file: bool,
    /// Addresses always permitted, even when [`block_private_hosts`](Self::block_private_hosts)
    /// is on — an explicit escape hatch for a known internal host (or a
    /// loopback test server). Checked before the SSRF filter.
    pub allowlist: Vec<IpAddr>,
    /// Optional jail for `file://` loads: resolved paths must stay within it.
    pub file_root: Option<PathBuf>,
    /// Maximum number of redirect hops before a load fails.
    pub max_redirects: u32,
    /// Per-request response-body byte cap (image/decompression bomb defense).
    pub max_response_bytes: u64,
    /// Per-page cumulative response-byte cap.
    pub max_total_bytes: u64,
    /// Per-page request-count cap.
    pub max_requests: u32,
    /// Maximum time to establish a single TCP connection (per resolved
    /// address). A hung `connect` fails with [`NetErrorKind::Timeout`] rather
    /// than blocking forever.
    ///
    /// [`NetErrorKind::Timeout`]: oxidepage_base::NetErrorKind::Timeout
    pub connect_timeout: Duration,
    /// Wall-clock cap on a whole fetch (DNS + connect + all redirect hops +
    /// body read/decompress). Exceeding it fails with
    /// [`NetErrorKind::Timeout`], so a slow or silent server can never hang a
    /// blocking fetch indefinitely.
    ///
    /// [`NetErrorKind::Timeout`]: oxidepage_base::NetErrorKind::Timeout
    pub request_timeout: Duration,
}

impl Default for ResourcePolicy {
    /// Secure defaults: HTTP(S) only, private hosts blocked, `file://` off.
    fn default() -> Self {
        Self {
            allowed_schemes: vec!["http".to_owned(), "https".to_owned()],
            block_private_hosts: true,
            allow_file: false,
            allowlist: Vec::new(),
            file_root: None,
            max_redirects: 20,
            max_response_bytes: 64 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_requests: 500,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
        }
    }
}

impl ResourcePolicy {
    /// Wraps the policy in an [`Arc`] for sharing with the connector.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Whether `scheme` (case-insensitive) is permitted.
    #[must_use]
    pub fn scheme_allowed(&self, scheme: &str) -> bool {
        self.allowed_schemes
            .iter()
            .any(|s| s.eq_ignore_ascii_case(scheme))
    }

    /// Whether a resolved IP may be connected to. Honors
    /// [`block_private_hosts`](Self::block_private_hosts): when off, all
    /// addresses are permitted (used by loopback test servers).
    #[must_use]
    pub fn ip_allowed(&self, ip: IpAddr) -> bool {
        if self.allowlist.contains(&ip) {
            return true;
        }
        if !self.block_private_hosts {
            return true;
        }
        ip_allowed(ip)
    }

    /// A permissive policy for the loopback test server: keeps the scheme
    /// allowlist and budgets but disables the private-host filter so
    /// `127.0.0.1` is reachable.
    #[must_use]
    pub fn permissive_localhost() -> Self {
        Self {
            block_private_hosts: false,
            ..Self::default()
        }
    }
}
