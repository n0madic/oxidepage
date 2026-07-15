//! A page-scoped cookie jar (design doc §5.5), RFC 6265bis.
//!
//! Implements domain/path matching, `Expires`/`Max-Age` (Max-Age wins),
//! `Secure`, `HttpOnly` (sent on HTTP requests but invisible to
//! `document.cookie`), `SameSite` (default `Lax`; `None` requires `Secure`),
//! the `__Host-`/`__Secure-` name prefixes, control-character rejection,
//! non-secure-overwrite protection, and per-domain + global caps with
//! oldest-first eviction. The Public Suffix List (`psl`) blocks supercookies
//! and backs registrable-domain (schemeful same-site) decisions.
//!
//! The jar is shared by document loads, scripts, fetch/XHR, and subresource
//! loads, so one page has one coherent cookie view.

use std::time::{Duration, SystemTime};

use url::Url;

/// Where a cookie operation originates. Script may neither read nor write
/// `HttpOnly` cookies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CookieSource {
    /// An HTTP request/response (`Set-Cookie` / `Cookie` headers).
    Http,
    /// Page script (`document.cookie`).
    Script,
}

/// The `SameSite` attribute value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SameSite {
    Strict,
    Lax,
    None,
}

/// Per-domain and global storage caps (RFC 6265bis §5.6 eviction).
const PER_DOMAIN_CAP: usize = 50;
const GLOBAL_CAP: usize = 3000;

/// RFC 6265bis §5.5: an expiry more than 400 days out is clamped to 400 days.
const MAX_COOKIE_LIFETIME: Duration = Duration::from_secs(400 * 24 * 60 * 60);

#[derive(Clone, Debug)]
struct Cookie {
    name: String,
    value: String,
    /// Canonicalized (lowercase, no leading dot) domain.
    domain: String,
    /// True when the cookie had no `Domain` attribute (host-only match).
    host_only: bool,
    path: String,
    /// `None` = session cookie (no persistence).
    expires: Option<SystemTime>,
    secure: bool,
    http_only: bool,
    same_site: SameSite,
    creation: SystemTime,
    last_access: SystemTime,
}

impl Cookie {
    fn is_expired(&self, now: SystemTime) -> bool {
        matches!(self.expires, Some(t) if t <= now)
    }
}

/// A page's cookie store.
#[derive(Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Processes a single `Set-Cookie` header value. Returns `false` if the
    /// cookie was rejected (malformed, policy violation). `source` gates
    /// `HttpOnly`: script may not set it.
    pub fn set_cookie(
        &mut self,
        url: &Url,
        header: &str,
        source: CookieSource,
        now: SystemTime,
    ) -> bool {
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return false;
        };
        let secure_request = url.scheme() == "https";

        let Some(parsed) = parse_set_cookie(header, &host, url, now) else {
            return false;
        };

        // Script may not set HttpOnly cookies (RFC 6265bis §8.6).
        if source == CookieSource::Script && parsed.http_only {
            return false;
        }
        // A non-secure request may not overwrite an existing secure cookie
        // with the same name that also domain-matches (§5.6).
        if !secure_request {
            let clashes_secure = self.cookies.iter().any(|c| {
                c.secure && c.name == parsed.name && domain_matches(&host, &c.domain, c.host_only)
            });
            if clashes_secure && !parsed.secure {
                return false;
            }
        }

        // Max-Age <= 0 / past Expires deletes any existing match.
        if parsed.is_expired(now) {
            self.cookies.retain(|c| {
                !(c.name == parsed.name && c.domain == parsed.domain && c.path == parsed.path)
            });
            return true;
        }

        // Insert or replace (same name+domain+path), preserving creation time.
        if let Some(existing) = self
            .cookies
            .iter_mut()
            .find(|c| c.name == parsed.name && c.domain == parsed.domain && c.path == parsed.path)
        {
            let creation = existing.creation;
            *existing = Cookie { creation, ..parsed };
        } else {
            self.cookies.push(parsed);
        }
        self.evict(now);
        true
    }

    /// Builds the `Cookie:` request-header value for `url`, or `None` if no
    /// cookie applies. `same_site` is whether the request is same-site to its
    /// initiator; `source` gates `HttpOnly` visibility.
    pub fn cookie_header(
        &mut self,
        url: &Url,
        same_site: bool,
        source: CookieSource,
        now: SystemTime,
    ) -> Option<String> {
        let matches = self.matching(url, same_site, source, now);
        if matches.is_empty() {
            return None;
        }
        Some(
            matches
                .iter()
                .map(|(n, v)| format!("{n}={v}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// The `document.cookie` getter value (same-site, script source).
    pub fn document_cookie(&mut self, url: &Url, now: SystemTime) -> String {
        self.cookie_header(url, true, CookieSource::Script, now)
            .unwrap_or_default()
    }

    /// The `document.cookie` setter (script source, one cookie).
    pub fn set_document_cookie(&mut self, url: &Url, header: &str, now: SystemTime) {
        let _ = self.set_cookie(url, header, CookieSource::Script, now);
    }

    /// Returns matching cookies as `(name, value)`, spec-ordered (longer path
    /// first, then older first), updating last-access times.
    fn matching(
        &mut self,
        url: &Url,
        same_site: bool,
        source: CookieSource,
        now: SystemTime,
    ) -> Vec<(String, String)> {
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return Vec::new();
        };
        let secure_request = url.scheme() == "https";
        let req_path = default_path(url);

        let mut indices: Vec<usize> = Vec::new();
        for (i, c) in self.cookies.iter().enumerate() {
            if c.is_expired(now) {
                continue;
            }
            if !domain_matches(&host, &c.domain, c.host_only) {
                continue;
            }
            if !path_matches(&req_path, &c.path) {
                continue;
            }
            if c.secure && !secure_request {
                continue;
            }
            if c.http_only && source == CookieSource::Script {
                continue;
            }
            let same_site_ok = match c.same_site {
                SameSite::None => true,
                SameSite::Lax | SameSite::Strict => same_site,
            };
            if !same_site_ok {
                continue;
            }
            indices.push(i);
        }
        // Longer paths first; ties broken by creation time (older first).
        indices.sort_by(|&a, &b| {
            let ca = &self.cookies[a];
            let cb = &self.cookies[b];
            cb.path
                .len()
                .cmp(&ca.path.len())
                .then(ca.creation.cmp(&cb.creation))
        });
        let mut out = Vec::with_capacity(indices.len());
        for i in indices {
            self.cookies[i].last_access = now;
            out.push((self.cookies[i].name.clone(), self.cookies[i].value.clone()));
        }
        out
    }

    /// Drops expired cookies and enforces the per-domain and global caps,
    /// evicting oldest-accessed first.
    fn evict(&mut self, now: SystemTime) {
        self.cookies.retain(|c| !c.is_expired(now));

        // Per-domain cap.
        let domains: Vec<String> = {
            let mut d: Vec<String> = self.cookies.iter().map(|c| c.domain.clone()).collect();
            d.sort();
            d.dedup();
            d
        };
        for domain in domains {
            let count = self.cookies.iter().filter(|c| c.domain == domain).count();
            if count > PER_DOMAIN_CAP {
                let mut idx: Vec<usize> = self
                    .cookies
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.domain == domain)
                    .map(|(i, _)| i)
                    .collect();
                idx.sort_by_key(|&i| self.cookies[i].last_access);
                let remove: std::collections::HashSet<usize> =
                    idx.into_iter().take(count - PER_DOMAIN_CAP).collect();
                let mut i = 0;
                self.cookies.retain(|_| {
                    let keep = !remove.contains(&i);
                    i += 1;
                    keep
                });
            }
        }

        // Global cap.
        while self.cookies.len() > GLOBAL_CAP {
            if let Some((i, _)) = self
                .cookies
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.last_access)
            {
                self.cookies.remove(i);
            } else {
                break;
            }
        }
    }

    /// Number of stored cookies (test/introspection aid).
    #[must_use]
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }
}

/// Parses a `Set-Cookie` header value into a normalized [`Cookie`], applying
/// prefix and attribute rules. Returns `None` if malformed or rejected.
fn parse_set_cookie(header: &str, host: &str, url: &Url, now: SystemTime) -> Option<Cookie> {
    let (pair, attrs) = match header.split_once(';') {
        Some((p, a)) => (p, a),
        None => (header, ""),
    };
    // Name-value pair must contain '='.
    let (name, value) = pair.split_once('=')?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() && value.is_empty() {
        return None;
    }
    // Control-character rejection on name and value.
    if has_ctl(name) || has_ctl(value) {
        return None;
    }
    // Value may be double-quoted; keep quotes stripped per common practice.
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);

    let mut domain: Option<String> = None;
    let mut path: Option<String> = None;
    let mut expires: Option<SystemTime> = None;
    let mut max_age: Option<i64> = None;
    let mut secure = false;
    let mut http_only = false;
    let mut same_site = SameSite::Lax;

    for attr in attrs.split(';') {
        let attr = attr.trim();
        if attr.is_empty() {
            continue;
        }
        let (key, val) = match attr.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (attr, ""),
        };
        match key.to_ascii_lowercase().as_str() {
            "domain" => {
                let d = val.trim_start_matches('.').to_ascii_lowercase();
                if !d.is_empty() {
                    domain = Some(d);
                }
            }
            "path" if val.starts_with('/') => path = Some(val.to_owned()),
            "expires" => {
                if let Some(t) = parse_cookie_date(val) {
                    expires = Some(t);
                }
            }
            "max-age" => {
                if let Ok(n) = val.parse::<i64>() {
                    max_age = Some(n);
                }
            }
            "secure" => secure = true,
            "httponly" => http_only = true,
            "samesite" => {
                same_site = match val.to_ascii_lowercase().as_str() {
                    "strict" => SameSite::Strict,
                    "none" => SameSite::None,
                    _ => SameSite::Lax,
                };
            }
            _ => {}
        }
    }

    // Max-Age precedence over Expires.
    let expires = if let Some(secs) = max_age {
        if secs <= 0 {
            // Already-expired sentinel (deletes on set).
            Some(now.checked_sub(Duration::from_secs(1)).unwrap_or(now))
        } else {
            // A huge Max-Age overflows `SystemTime`; treat it as a session
            // cookie (`None`) rather than panicking.
            now.checked_add(Duration::from_secs(secs as u64))
        }
    } else {
        expires
    };
    // Clamp a far-future expiry to the 400-day cap (RFC 6265bis §5.5). If the
    // cap itself overflows, leave the expiry as-is.
    let expires = match (expires, now.checked_add(MAX_COOKIE_LIFETIME)) {
        (Some(exp), Some(cap)) if exp > cap => Some(cap),
        (exp, _) => exp,
    };

    // Domain handling + Public Suffix guard.
    let (domain, host_only) = match domain {
        Some(d) => {
            if is_public_suffix(&d) {
                // Only allowed if it exactly equals the host, and then it's
                // host-only (§5.4). Otherwise reject (supercookie).
                if d == host {
                    (host.to_owned(), true)
                } else {
                    return None;
                }
            } else if domain_matches(host, &d, false) {
                (d, false)
            } else {
                // Domain attribute must domain-match the request host.
                return None;
            }
        }
        None => (host.to_owned(), true),
    };

    let path = path.unwrap_or_else(|| default_path(url));

    // `None` SameSite requires Secure (schemeful, §5.4).
    if same_site == SameSite::None && !secure {
        return None;
    }

    // Cookie name prefixes (§5.5).
    if let Some(stripped) = name.strip_prefix("__Host-") {
        let _ = stripped;
        if !secure || !host_only || path != "/" {
            return None;
        }
    } else if name.strip_prefix("__Secure-").is_some() && !secure {
        return None;
    }

    let secure_request = url.scheme() == "https";
    // A `Secure` cookie may only be set over a secure connection... except
    // permit script/document over http for test locality is *not* granted —
    // spec requires secure transport. But loopback http test servers set
    // Secure rarely; we honor the spec.
    if secure && !secure_request {
        return None;
    }

    Some(Cookie {
        name: name.to_owned(),
        value: value.to_owned(),
        domain,
        host_only,
        path,
        expires,
        secure,
        http_only,
        same_site,
        creation: now,
        last_access: now,
    })
}

/// RFC 6265 §5.1.3 domain matching. IP-literal hosts match only exactly.
fn domain_matches(host: &str, domain: &str, host_only: bool) -> bool {
    if host == domain {
        return true;
    }
    if host_only {
        return false;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    host.len() > domain.len()
        && host.ends_with(domain)
        && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
}

/// RFC 6265 §5.1.4 path matching.
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if !request_path.starts_with(cookie_path) {
        return false;
    }
    cookie_path.ends_with('/') || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')
}

/// RFC 6265 §5.1.4 default path from a request URI.
fn default_path(url: &Url) -> String {
    let path = url.path();
    if path.is_empty() || !path.starts_with('/') {
        return "/".to_owned();
    }
    match path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(i) => path[..i].to_owned(),
    }
}

fn is_public_suffix(domain: &str) -> bool {
    match psl::suffix(domain.as_bytes()) {
        Some(s) => s.as_bytes().eq_ignore_ascii_case(domain.as_bytes()),
        None => false,
    }
}

/// The registrable ("effective TLD + 1") domain, for schemeful same-site.
#[must_use]
pub fn registrable_domain(host: &str) -> Option<String> {
    psl::domain(host.as_bytes()).map(|d| String::from_utf8_lossy(d.as_bytes()).into_owned())
}

fn has_ctl(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20 || b == 0x7f)
}

/// RFC 6265 §5.1.1 cookie-date parsing (tolerant), returning a UTC instant.
fn parse_cookie_date(s: &str) -> Option<SystemTime> {
    let mut hour = None;
    let mut minute = None;
    let mut second = None;
    let mut day = None;
    let mut month = None;
    let mut year = None;

    for token in s.split(|c: char| {
        c == ' ' || c == '\t' || c == ',' || c == ';' || (c.is_ascii_punctuation() && c != ':')
    }) {
        if token.is_empty() {
            continue;
        }
        // time: h:m:s
        if hour.is_none() && token.matches(':').count() == 2 {
            let parts: Vec<&str> = token.splitn(3, ':').collect();
            if let (Ok(h), Ok(mi), Ok(se)) = (
                parts[0].parse::<u32>(),
                parts[1].parse::<u32>(),
                // seconds may trail with non-digits already split off
                parts[2].parse::<u32>(),
            ) {
                hour = Some(h);
                minute = Some(mi);
                second = Some(se);
                continue;
            }
        }
        if month.is_none()
            && let Some(m) = parse_month(token)
        {
            month = Some(m);
            continue;
        }
        if day.is_none()
            && let Ok(d) = token.parse::<u32>()
            && (1..=31).contains(&d)
        {
            day = Some(d);
            continue;
        }
        if year.is_none()
            && let Ok(y) = token.parse::<i64>()
        {
            year = Some(y);
            continue;
        }
    }

    let (h, mi, se) = (hour?, minute?, second?);
    let d = day?;
    let m = month?;
    let mut y = year?;
    if (70..=99).contains(&y) {
        y += 1900;
    } else if (0..=69).contains(&y) {
        y += 2000;
    }
    if h > 23 || mi > 59 || se > 59 {
        return None;
    }
    // Every step is checked: a crafted far-future/past year must yield `None`
    // (an invalid date), never overflow-panic and poison the cookie jar mutex.
    let days = days_from_civil(y, m, i64::from(d))?;
    let secs = days
        .checked_mul(86400)?
        .checked_add(i64::from(h) * 3600)?
        .checked_add(i64::from(mi) * 60)?
        .checked_add(i64::from(se))?;
    if secs < 0 {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    } else {
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
    }
}

fn parse_month(token: &str) -> Option<i64> {
    if token.len() < 3 {
        return None;
    }
    Some(match token[..3].to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    })
}

/// Days from the Unix epoch to `y-m-d` (Howard Hinnant's algorithm). Returns
/// `None` on any arithmetic overflow (an absurd year), so an attacker-supplied
/// `Expires` can never overflow-panic.
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    let y = if m <= 2 { y.checked_sub(1)? } else { y };
    let era = (if y >= 0 { y } else { y.checked_sub(399)? }) / 400;
    let yoe = y.checked_sub(era.checked_mul(400)?)?;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe
        .checked_mul(365)?
        .checked_add(yoe / 4)?
        .checked_sub(yoe / 100)?
        .checked_add(doy)?;
    era.checked_mul(146097)?
        .checked_add(doe)?
        .checked_sub(719468)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }
    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn basic_set_and_send() {
        let mut jar = CookieJar::new();
        let u = url("http://example.com/");
        assert!(jar.set_cookie(&u, "sid=abc", CookieSource::Http, t0()));
        assert_eq!(
            jar.cookie_header(&u, true, CookieSource::Http, t0())
                .as_deref(),
            Some("sid=abc")
        );
    }

    #[test]
    fn httponly_invisible_to_script() {
        let mut jar = CookieJar::new();
        let u = url("http://example.com/");
        jar.set_cookie(&u, "a=1; HttpOnly", CookieSource::Http, t0());
        jar.set_cookie(&u, "b=2", CookieSource::Http, t0());
        // HTTP sees both; script sees only b.
        assert_eq!(
            jar.cookie_header(&u, true, CookieSource::Http, t0())
                .as_deref(),
            Some("a=1; b=2")
        );
        assert_eq!(jar.document_cookie(&u, t0()), "b=2");
        // Script cannot set HttpOnly.
        assert!(!jar.set_cookie(&u, "c=3; HttpOnly", CookieSource::Script, t0()));
    }

    #[test]
    fn domain_and_path_matching() {
        let mut jar = CookieJar::new();
        jar.set_cookie(
            &url("http://example.com/"),
            "d=1; Domain=example.com",
            CookieSource::Http,
            t0(),
        );
        jar.set_cookie(
            &url("http://example.com/app/"),
            "p=2; Path=/app",
            CookieSource::Http,
            t0(),
        );
        // Subdomain gets the domain cookie but not the path cookie under /.
        let sub = url("http://www.example.com/");
        assert_eq!(
            jar.cookie_header(&sub, true, CookieSource::Http, t0())
                .as_deref(),
            Some("d=1")
        );
        // /app path gets both, longer path first.
        let app = url("http://example.com/app/x");
        assert_eq!(
            jar.cookie_header(&app, true, CookieSource::Http, t0())
                .as_deref(),
            Some("p=2; d=1")
        );
    }

    #[test]
    fn public_suffix_domain_rejected() {
        let mut jar = CookieJar::new();
        // Cannot set a cookie for the whole `.com` public suffix.
        assert!(!jar.set_cookie(
            &url("http://example.com/"),
            "x=1; Domain=com",
            CookieSource::Http,
            t0()
        ));
    }

    #[test]
    fn secure_requires_https_and_samesite_none() {
        let mut jar = CookieJar::new();
        // Secure over http is rejected.
        assert!(!jar.set_cookie(
            &url("http://example.com/"),
            "s=1; Secure",
            CookieSource::Http,
            t0()
        ));
        // SameSite=None without Secure rejected.
        assert!(!jar.set_cookie(
            &url("https://example.com/"),
            "n=1; SameSite=None",
            CookieSource::Http,
            t0()
        ));
        // With Secure, ok.
        assert!(jar.set_cookie(
            &url("https://example.com/"),
            "n=1; SameSite=None; Secure",
            CookieSource::Http,
            t0()
        ));
    }

    #[test]
    fn host_prefix_rules() {
        let mut jar = CookieJar::new();
        let u = url("https://example.com/");
        // __Host- requires Secure, host-only, Path=/.
        assert!(jar.set_cookie(&u, "__Host-a=1; Secure; Path=/", CookieSource::Http, t0()));
        assert!(!jar.set_cookie(&u, "__Host-b=1; Secure; Path=/x", CookieSource::Http, t0()));
        assert!(!jar.set_cookie(
            &u,
            "__Host-c=1; Secure; Domain=example.com; Path=/",
            CookieSource::Http,
            t0()
        ));
    }

    #[test]
    fn samesite_strict_not_sent_cross_site() {
        let mut jar = CookieJar::new();
        let u = url("https://example.com/");
        jar.set_cookie(
            &u,
            "strict=1; SameSite=Strict; Secure",
            CookieSource::Http,
            t0(),
        );
        jar.set_cookie(
            &u,
            "none=1; SameSite=None; Secure",
            CookieSource::Http,
            t0(),
        );
        // Cross-site: only the None cookie is sent.
        assert_eq!(
            jar.cookie_header(&u, false, CookieSource::Http, t0())
                .as_deref(),
            Some("none=1")
        );
    }

    #[test]
    fn max_age_zero_deletes() {
        let mut jar = CookieJar::new();
        let u = url("http://example.com/");
        jar.set_cookie(&u, "a=1", CookieSource::Http, t0());
        assert_eq!(jar.len(), 1);
        jar.set_cookie(&u, "a=1; Max-Age=0", CookieSource::Http, t0());
        assert_eq!(jar.len(), 0);
    }

    #[test]
    fn expires_in_past_not_sent() {
        let mut jar = CookieJar::new();
        let u = url("http://example.com/");
        jar.set_cookie(
            &u,
            "a=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            CookieSource::Http,
            t0(),
        );
        assert!(
            jar.cookie_header(&u, true, CookieSource::Http, t0())
                .is_none()
        );
    }

    #[test]
    fn control_chars_rejected() {
        let mut jar = CookieJar::new();
        let u = url("http://example.com/");
        assert!(!jar.set_cookie(&u, "a=va\x00lue", CookieSource::Http, t0()));
    }

    #[test]
    fn cookie_date_parses() {
        let t = parse_cookie_date("Wed, 15 Nov 2023 12:00:00 GMT").unwrap();
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1_700_049_600);
    }

    #[test]
    fn non_secure_cannot_overwrite_secure() {
        let mut jar = CookieJar::new();
        jar.set_cookie(
            &url("https://example.com/"),
            "a=secure; Secure",
            CookieSource::Http,
            t0(),
        );
        // A plain-http set with the same name must not clobber it.
        assert!(!jar.set_cookie(
            &url("http://example.com/"),
            "a=plain",
            CookieSource::Http,
            t0()
        ));
        assert_eq!(
            jar.cookie_header(&url("https://example.com/"), true, CookieSource::Http, t0())
                .as_deref(),
            Some("a=secure")
        );
    }

    #[test]
    fn absurd_expires_year_does_not_panic() {
        let mut jar = CookieJar::new();
        let u = url("http://example.com/");
        // A crafted far-future year previously overflow-panicked the date math
        // (and poisoned the jar mutex). It must now be handled gracefully.
        assert!(jar.set_cookie(
            &u,
            "a=1; Expires=Fri, 01 Jan 300000000000 00:00:00 GMT",
            CookieSource::Http,
            t0(),
        ));
        // The overflowing `Expires` is dropped → session cookie, still stored.
        assert_eq!(
            jar.cookie_header(&u, true, CookieSource::Http, t0())
                .as_deref(),
            Some("a=1")
        );
        // The jar remains fully usable for subsequent operations.
        assert!(jar.set_cookie(&u, "b=2", CookieSource::Http, t0()));
        assert_eq!(
            jar.cookie_header(&u, true, CookieSource::Http, t0())
                .as_deref(),
            Some("a=1; b=2")
        );
    }

    #[test]
    fn absurd_negative_expires_year_does_not_panic() {
        let mut jar = CookieJar::new();
        let u = url("http://example.com/");
        // A huge negative year drives the epoch-seconds math below i64::MIN.
        assert!(jar.set_cookie(
            &u,
            "a=1; Expires=Fri, 01 Jan -300000000000 00:00:00 GMT",
            CookieSource::Http,
            t0(),
        ));
        assert!(jar.set_cookie(&u, "b=2", CookieSource::Http, t0()));
    }

    #[test]
    fn far_future_expiry_capped_to_400_days() {
        let mut jar = CookieJar::new();
        let u = url("https://example.com/");
        // Max-Age far beyond the 400-day cap must be clamped, not honored.
        assert!(jar.set_cookie(
            &u,
            "a=1; Max-Age=99999999999; Secure",
            CookieSource::Http,
            t0(),
        ));
        let cap = t0() + MAX_COOKIE_LIFETIME;
        // Just before the cap the cookie is live; just after it is expired.
        assert!(
            jar.cookie_header(&u, true, CookieSource::Http, cap - Duration::from_secs(10))
                .is_some()
        );
        assert!(
            jar.cookie_header(&u, true, CookieSource::Http, cap + Duration::from_secs(10))
                .is_none()
        );
    }
}
