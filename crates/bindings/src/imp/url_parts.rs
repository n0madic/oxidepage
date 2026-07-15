//! URL decomposition getters/setters, shared by the `URL` interface and the
//! `HTMLHyperlinkElementUtils` mixin so the two cannot drift.
//!
//! Setters swallow `url::Url`'s `Result`: a rejected component leaves the URL
//! untouched, which is exactly the URL standard's "on failure, do nothing".
//!
//! A setter re-runs the basic URL parser with a **state override**, which is not
//! the same as parsing a whole URL:
//!
//! * tab/LF/CR are removed from the value first, but leading and trailing C0
//!   controls and spaces are *not* trimmed — that step is skipped whenever a
//!   state override is given, so `protocol = "https "` is a failure, not `https`;
//! * a state's terminator ends the parse *successfully* instead of advancing to
//!   the next state, so trailing junk is ignored rather than rejected
//!   (`port = "8080stuff2"` is 8080, `host = "a.com/x"` is `a.com`).
//!
//! `url::Url`'s setters implement the component grammars; the code below is the
//! state-override layer on top of them.

use std::borrow::Cow;

use url::Url;

/// The basic URL parser removes every U+0009 TAB, U+000A LF and U+000D CR from
/// its input before looking at it.
fn strip_tabs_newlines(value: &str) -> Cow<'_, str> {
    if value.contains(['\t', '\n', '\r']) {
        Cow::Owned(value.replace(['\t', '\n', '\r'], ""))
    } else {
        Cow::Borrowed(value)
    }
}

/// The URL standard's special schemes. `url` knows this internally but does not
/// expose it, and `\` is a path delimiter for these and only these.
fn is_special(url: &Url) -> bool {
    matches!(
        url.scheme(),
        "http" | "https" | "ws" | "wss" | "ftp" | "file"
    )
}

/// Whether the serialization has an authority (`//`) after the scheme. `Url`
/// folds "null host" and "empty host" into one state — `has_host()` is false for
/// both `foo:/p` and `foo:///p` — and the path setter has to tell them apart.
fn has_authority(url: &Url) -> bool {
    url.as_str()[url.scheme().len() + 1..].starts_with("//")
}

/// The slice of a host value the parser actually consumes: everything before the
/// first delimiter that ends the host state.
fn host_buffer<'a>(url: &Url, value: &'a str) -> &'a str {
    let special = is_special(url);
    let end = value
        .find(|c| matches!(c, '/' | '?' | '#') || (c == '\\' && special))
        .unwrap_or(value.len());
    &value[..end]
}

/// Split a host buffer at the port separator: the first `:` outside an IPv6
/// literal's brackets.
fn split_port(buffer: &str) -> (&str, Option<&str>) {
    let mut inside_brackets = false;
    for (i, c) in buffer.char_indices() {
        match c {
            '[' => inside_brackets = true,
            ']' => inside_brackets = false,
            ':' if !inside_brackets => return (&buffer[..i], Some(&buffer[i + 1..])),
            _ => {}
        }
    }
    (buffer, None)
}

/// The port the port state would accept: the leading run of ASCII digits, which
/// any other code point terminates *successfully* under a state override.
/// `None` — no digits, or a value past 2^16-1 — means "leave the port alone".
fn leading_port(value: &str) -> Option<u16> {
    let end = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    value[..end].parse().ok()
}

/// Shared tail of the host and hostname setters. Returns whether the host was
/// installed; on failure the URL is untouched.
fn set_host_buffer(url: &mut Url, buffer: &str) -> bool {
    // "If state override is given, buffer is the empty string, and either url
    // includes credentials or url's port is non-null, then return." (An empty
    // host under a special non-file scheme is rejected by `set_host` itself.)
    if buffer.is_empty()
        && (!url.username().is_empty() || url.password().is_some() || url.port().is_some())
    {
        return false;
    }
    // The file host state accepts the empty host and folds `localhost` into it.
    // `Url::set_host` rejects `Some("")` as an empty domain, so `None` — its
    // spelling of "drop the host" — is what leaves the `file:///` behind.
    let is_file = url.scheme() == "file";
    if is_file && buffer.is_empty() {
        return url.set_host(None).is_ok();
    }
    if url.set_host(Some(buffer)).is_err() {
        return false;
    }
    if is_file && url.host_str() == Some("localhost") {
        let _ = url.set_host(None);
    }
    true
}

#[must_use]
pub(crate) fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

#[must_use]
pub(crate) fn protocol(url: &Url) -> String {
    format!("{}:", url.scheme())
}

pub(crate) fn set_protocol(url: &mut Url, value: &str) {
    let value = strip_tabs_newlines(value);
    // Scheme state: the first `:` ends the scheme and the rest is ignored, so
    // `view-source+data:foo : bar` sets `view-source+data`. `set_scheme` enforces
    // the grammar and the special/non-special and file constraints.
    let scheme = value.split_once(':').map_or(&*value, |(scheme, _)| scheme);
    let _ = url.set_scheme(scheme);
}

#[must_use]
pub(crate) fn username(url: &Url) -> String {
    url.username().to_owned()
}

/// The username and password setters do not run the parser at all — they neither
/// strip tab/LF/CR nor stop at a delimiter, they only percent-encode. `set_username`
/// rejects a URL that cannot have credentials (no host, empty host, or `file:`).
pub(crate) fn set_username(url: &mut Url, value: &str) {
    let _ = url.set_username(value);
}

#[must_use]
pub(crate) fn password(url: &Url) -> String {
    url.password().unwrap_or("").to_owned()
}

pub(crate) fn set_password(url: &mut Url, value: &str) {
    let password = (!value.is_empty()).then_some(value);
    let _ = url.set_password(password);
}

#[must_use]
pub(crate) fn host(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_owned(),
        (None, _) => String::new(),
    }
}

pub(crate) fn set_host(url: &mut Url, value: &str) {
    // A URL with an opaque path has no host to set.
    if url.cannot_be_a_base() {
        return;
    }
    let value = strip_tabs_newlines(value);
    let (hostname, port) = split_port(host_buffer(url, &value));

    // The file host state parses no port and treats `:` as a forbidden host code
    // point, so `file://y/`.host = "x:123" fails. (`Url::set_host` would instead
    // truncate at the `:` and keep `x`.)
    if port.is_some() && (hostname.is_empty() || url.scheme() == "file") {
        return;
    }
    if !set_host_buffer(url, hostname) {
        return;
    }
    // The port state is only reached through an explicit `:`. An empty or
    // unparseable port leaves the existing one alone — the host stays set either
    // way, as the parser mutates the URL in place before it fails.
    if let Some(port) = port.and_then(leading_port) {
        let _ = url.set_port(Some(port));
    }
}

#[must_use]
pub(crate) fn hostname(url: &Url) -> String {
    url.host_str().unwrap_or("").to_owned()
}

pub(crate) fn set_hostname(url: &mut Url, value: &str) {
    if url.cannot_be_a_base() {
        return;
    }
    let value = strip_tabs_newlines(value);
    let buffer = host_buffer(url, &value);
    // Unlike the host state, the hostname state override returns at the port
    // separator *without setting anything*: `hostname = "example.com:8080"` is a
    // no-op, not a truncation to `example.com`.
    if split_port(buffer).1.is_some() {
        return;
    }
    set_host_buffer(url, buffer);
}

#[must_use]
pub(crate) fn port(url: &Url) -> String {
    url.port().map(|p| p.to_string()).unwrap_or_default()
}

pub(crate) fn set_port(url: &mut Url, value: &str) {
    // The emptiness test is on the raw value, before stripping: `port = "\n\t"`
    // is *not* the empty string, so it runs the port state, which sees an empty
    // buffer and leaves the port unchanged.
    if value.is_empty() {
        let _ = url.set_port(None);
        return;
    }
    // `set_port` rejects URLs that cannot have a port and nulls a port that is
    // the new scheme's default.
    if let Some(port) = leading_port(&strip_tabs_newlines(value)) {
        let _ = url.set_port(Some(port));
    }
}

#[must_use]
pub(crate) fn pathname(url: &Url) -> String {
    url.path().to_owned()
}

pub(crate) fn set_pathname(url: &mut Url, value: &str) {
    // An opaque path (`data:`, `mailto:`) cannot be replaced.
    if url.cannot_be_a_base() {
        return;
    }
    let path = strip_tabs_newlines(value);
    // `Url::set_path` assumes the URL has a host (a FIXME in `url`). It doesn't
    // when there is no authority, and then the path start state appends an empty
    // segment: a path-only URL keeps its leading slash instead of losing the path.
    if path.is_empty() && !has_authority(url) {
        url.set_path("/");
        return;
    }
    url.set_path(&path);
}

#[must_use]
pub(crate) fn search(url: &Url) -> String {
    match url.query() {
        Some(q) if !q.is_empty() => format!("?{q}"),
        _ => String::new(),
    }
}

pub(crate) fn set_search(url: &mut Url, value: &str) {
    // The empty string clears the query; a lone `?` sets an *empty* query, which
    // still serializes its delimiter. Both getters report "".
    if value.is_empty() {
        url.set_query(None);
        return;
    }
    // The single leading `?` is removed from the raw value; `set_query` strips
    // tab/LF/CR itself.
    url.set_query(Some(value.strip_prefix('?').unwrap_or(value)));
}

#[must_use]
pub(crate) fn hash(url: &Url) -> String {
    match url.fragment() {
        Some(f) if !f.is_empty() => format!("#{f}"),
        _ => String::new(),
    }
}

pub(crate) fn set_hash(url: &mut Url, value: &str) {
    // As for `search`: "" clears the fragment, "#" sets an empty one.
    if value.is_empty() {
        url.set_fragment(None);
        return;
    }
    let input = value.strip_prefix('#').unwrap_or(value);
    url.set_fragment(Some(&strip_tabs_newlines(input)));
}
