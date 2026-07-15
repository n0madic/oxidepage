//! Regression tests for the URL decomposition setters in
//! `crates/bindings/src/imp/url_parts.rs`, shared by the `URL` interface and the
//! `HTMLHyperlinkElementUtils` mixin (`<a>` / `<area>`).
//!
//! A setter re-runs the basic URL parser with a *state override*: the value is
//! stripped of tab/LF/CR but not trimmed, and a delimiter ends the parse
//! successfully instead of failing it. The setters used to hand the raw value
//! straight to `url::Url`, which implements neither rule. Every case below is
//! lifted from the URL Standard's own test data,
//! `tests/wpt/vendor/url/resources/setters_tests.json`.

use oxidepage_page::{Page, PageOptions};

/// `new URL(href)`, then `url[property] = value`; returns the resulting `href`.
fn set(page: &Page, href: &str, property: &str, value: &str) -> String {
    read(page, href, property, value, "href")
}

/// As [`set`], but reads back `getter` instead of `href`.
fn read(page: &Page, href: &str, property: &str, value: &str, getter: &str) -> String {
    let source = format!(
        "(() => {{ const u = new URL({}); u.{property} = {}; return u.{getter}; }})()",
        js_string(href),
        js_string(value),
    );
    page.eval_to_string(&source).expect("eval")
}

fn js_string(value: &str) -> String {
    let mut out = String::from("'");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// The parser removes every tab/LF/CR from the value before looking at it, so a
/// split-up value still parses. It does *not* trim spaces or other C0 controls:
/// trimming only happens when no state override is given, i.e. in the constructor.
#[test]
fn tab_and_newline_are_stripped_from_the_value() {
    let page = Page::new(PageOptions::default()).unwrap();

    assert_eq!(
        set(&page, "http://test/", "protocol", "h\r\ntt\tps"),
        "https://test/"
    );
    assert_eq!(
        set(&page, "https://test.invalid/", "hostname", "foo\t\r\nbar"),
        "https://foobar/"
    );
    assert_eq!(
        read(
            &page,
            "https://domain.com:3000",
            "port",
            "\n\t80\n\t80\n\t",
            "port"
        ),
        "8080"
    );
    assert_eq!(
        set(&page, "https://example.net", "pathname", "te\tst"),
        "https://example.net/test"
    );
    assert_eq!(
        set(&page, "https://example.net", "search", "te\nst"),
        "https://example.net/?test"
    );
    assert_eq!(
        set(&page, "https://example.net", "hash", "te\rst"),
        "https://example.net/#test"
    );

    // Every other C0 control, and a space, remains a parse failure.
    assert_eq!(
        set(&page, "http://test/", "protocol", "https\0"),
        "http://test/"
    );
    assert_eq!(
        set(&page, "http://test/", "protocol", "https "),
        "http://test/"
    );

    // The credentials setters run no parser at all: they percent-encode the raw
    // value, tabs and newlines included.
    assert_eq!(
        read(
            &page,
            "http://example.net",
            "username",
            "te\tst",
            "username"
        ),
        "te%09st"
    );
}

/// Host state: the value is cut at the first path/query/fragment delimiter (and
/// at `\` for a special scheme), then split at the port separator — the first `:`
/// outside an IPv6 literal's brackets.
#[test]
fn host_setter_truncates_at_a_delimiter_and_parses_the_port() {
    let page = Page::new(PageOptions::default()).unwrap();

    let base = "http://example.net/path";
    assert_eq!(
        set(&page, base, "host", "example.com:8080/stuff"),
        "http://example.com:8080/path"
    );
    assert_eq!(
        set(&page, base, "host", "example.com#stuff"),
        "http://example.com/path"
    );
    assert_eq!(
        set(&page, base, "host", "example.com?stuff"),
        "http://example.com/path"
    );
    assert_eq!(
        set(&page, base, "host", "example.com\\stuff"),
        "http://example.com/path"
    );
    // `\` is not a delimiter for a non-special scheme — just a forbidden host code point.
    assert_eq!(
        set(
            &page,
            "view-source+http://example.net/path",
            "host",
            "example.com\\stuff"
        ),
        "view-source+http://example.net/path"
    );

    // The port separator is the first `:` outside brackets, so an IPv6 literal
    // keeps its colons.
    assert_eq!(
        set(&page, "http://example.net", "host", "[2001:db8::2]:4002"),
        "http://[2001:db8::2]:4002/"
    );
    // A port that does not parse leaves the old port alone; the host is still set.
    assert_eq!(
        set(
            &page,
            "http://example.net:8080/test",
            "host",
            "[::1]:invalid"
        ),
        "http://[::1]:8080/test"
    );
    assert_eq!(
        set(&page, base, "host", "example.com:65536"),
        "http://example.com/path"
    );
    // A leading `:` is an empty host buffer: a parse failure, not an empty host.
    assert_eq!(set(&page, "foo://path/to", "host", ":80"), "foo://path/to");

    // A URL with an opaque path has no host to set.
    assert_eq!(
        set(&page, "data:text/plain,Stuff", "host", "example.net"),
        "data:text/plain,Stuff"
    );

    // The file host state parses no port (`:` is forbidden there) and folds
    // `localhost` into the empty host.
    assert_eq!(set(&page, "file://y/", "host", "x:123"), "file://y/");
    assert_eq!(set(&page, "file://y/", "host", "loc%41lhost"), "file:///");
    assert_eq!(set(&page, "file://hi/x", "host", ""), "file:///x");
}

/// Hostname state is *not* host state: it returns at the port separator without
/// setting anything, rather than truncating there.
#[test]
fn hostname_setter_rejects_a_port_instead_of_truncating() {
    let page = Page::new(PageOptions::default()).unwrap();

    assert_eq!(
        set(
            &page,
            "http://example.net/path",
            "hostname",
            "example.com:8080"
        ),
        "http://example.net/path"
    );
    assert_eq!(
        set(
            &page,
            "http://example.net:8080/path",
            "hostname",
            "example.com:"
        ),
        "http://example.net:8080/path"
    );
    // The same value through `host` does set both components.
    assert_eq!(
        set(&page, "http://example.net/path", "host", "example.com:8080"),
        "http://example.com:8080/path"
    );

    // Delimiters still truncate, so a `:` after one is already gone.
    assert_eq!(
        set(&page, "https://test.invalid/", "hostname", "test/:aaa"),
        "https://test/"
    );
    // An empty host is refused while the URL has credentials or a port.
    assert_eq!(
        set(&page, "sc://test@test/", "hostname", ""),
        "sc://test@test/"
    );
    assert_eq!(set(&page, "sc://test:12/", "hostname", ""), "sc://test:12/");
}

/// Port state under a state override: the first non-digit terminates it
/// *successfully*, so trailing junk is ignored rather than rejected.
#[test]
fn port_setter_takes_the_leading_digits() {
    let page = Page::new(PageOptions::default()).unwrap();

    let base = "http://example.net:8080/path";
    assert_eq!(read(&page, base, "port", "8080stuff2", "port"), "8080");
    assert_eq!(read(&page, base, "port", "8080+2", "port"), "8080");
    assert_eq!(read(&page, base, "port", "4wpt", "port"), "4");
    // No leading digit at all, and an overflow, both leave the port unchanged.
    assert_eq!(read(&page, base, "port", "randomstring", "port"), "8080");
    assert_eq!(read(&page, base, "port", "65536", "port"), "8080");
    // The empty string — tested before stripping — is the one value that nulls it.
    assert_eq!(set(&page, base, "port", ""), "http://example.net/path");
    // "\n\t" is not the empty string: it runs the port state, which sees an
    // empty buffer and keeps the port.
    assert_eq!(read(&page, base, "port", "\n\t", "port"), "8080");
    // A scheme-default port is dropped.
    assert_eq!(set(&page, base, "port", "80"), "http://example.net/path");
}

/// Scheme state ends at the first `:`; the rest of the value is ignored.
#[test]
fn protocol_setter_stops_at_the_first_colon() {
    let page = Page::new(PageOptions::default()).unwrap();

    assert_eq!(
        set(
            &page,
            "data:text/html,<p>Test",
            "protocol",
            "view-source+data:foo : bar"
        ),
        "view-source+data:text/html,<p>Test"
    );
    assert_eq!(
        set(&page, "http://example.net", "protocol", "https:foo : bar"),
        "https://example.net/"
    );
    // The special/non-special and file constraints still reject the value.
    assert_eq!(
        set(&page, "http://example.net", "protocol", "b"),
        "http://example.net/"
    );
    assert_eq!(
        set(&page, "a://example.net", "protocol", "0b"),
        "a://example.net"
    );
}

/// An opaque path (`data:`, `mailto:`) cannot be replaced.
#[test]
fn pathname_setter_ignores_opaque_paths() {
    let page = Page::new(PageOptions::default()).unwrap();

    assert_eq!(
        set(&page, "data:original", "pathname", "new value"),
        "data:original"
    );
    assert_eq!(
        set(&page, "mailto:me@example.net", "pathname", "/foo"),
        "mailto:me@example.net"
    );
    // A non-opaque path is still replaced, `..` segments and all.
    assert_eq!(
        set(&page, "https://example.net#nav", "pathname", "../home"),
        "https://example.net/home#nav"
    );
}

/// `""` clears the component; a bare `"?"` / `"#"` sets an *empty* one, which
/// still serializes its delimiter. Both getters report `""` either way.
#[test]
fn empty_search_and_hash_differ_from_a_bare_delimiter() {
    let page = Page::new(PageOptions::default()).unwrap();

    let base = "https://example.net?lang=en-US#nav";
    assert_eq!(set(&page, base, "search", "?"), "https://example.net/?#nav");
    assert_eq!(read(&page, base, "search", "?", "search"), "");
    assert_eq!(set(&page, base, "search", ""), "https://example.net/#nav");

    assert_eq!(
        set(&page, base, "hash", "#"),
        "https://example.net/?lang=en-US#"
    );
    assert_eq!(read(&page, base, "hash", "#", "hash"), "");
    assert_eq!(
        set(&page, base, "hash", ""),
        "https://example.net/?lang=en-US"
    );

    // A single leading delimiter is removed, further ones are content.
    assert_eq!(
        set(&page, base, "search", "??lang=fr"),
        "https://example.net/??lang=fr#nav"
    );
    assert_eq!(
        set(&page, "https://example.net?lang=en-US", "hash", "##nav"),
        "https://example.net/?lang=en-US##nav"
    );
}

/// `url_parts` backs the `HTMLHyperlinkElementUtils` mixin too, so `<a>` gets the
/// same parser rules as `URL` — that sharing is the point of the module.
#[test]
fn anchor_element_shares_the_setters() {
    let page = Page::new(PageOptions::default()).unwrap();

    assert_eq!(
        page.eval_to_string(
            "(() => {
               const a = document.createElement('a');
               a.href = 'http://example.net/path';
               a.host = 'example.com:8080/stuff';
               a.port = '9090stuff';
               a.hash = '#';
               return a.href;
             })()"
        )
        .expect("eval"),
        "http://example.com:9090/path#"
    );
}
