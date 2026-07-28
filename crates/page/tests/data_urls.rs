//! `data:` URL subresources: classic scripts (parser-blocking, `defer`,
//! `async`, dynamically inserted), ES modules, `<link rel=stylesheet>` and
//! `@import`.
//!
//! `data:` is decoded at the top of the fetch pipeline beside `file://`
//! (`oxidepage_net::data`), so these all travel their ordinary code paths —
//! which is what keeps the asynchronous ones asynchronous. The regressions
//! pinned here are the two halves of the bug this fixes: `data:` reaching the
//! scheme allowlist and being rejected as "scheme `data` is not allowed", and
//! base64 bodies being decoded before percent-decoding.

use std::time::Duration;

use oxidepage_page::{Page, PageOptions, load_html_page};

fn eval_string(page: &Page, source: &str) -> String {
    page.eval_to_string(source).expect("eval")
}

fn load(html: &str) -> Page {
    let page = load_html_page(html, PageOptions::default()).expect("load");
    page.settle(Duration::from_secs(2));
    page
}

/// No script error was reported — the assertion that actually regresses when a
/// scheme gate rejects `data:`, since a blocked subresource is non-fatal and
/// the page otherwise loads fine.
fn assert_no_errors(page: &Page) {
    let errors: Vec<String> = page.drain_errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "unexpected script errors: {errors:?}");
}

/// The five spellings Acid3 uses, verbatim — plain, base64, fully
/// percent-encoded base64, base64 padded with percent-encoded whitespace, and a
/// plain body containing an escaped quote.
#[test]
fn acid3_data_url_script_spellings() {
    let page = load(
        r#"<!doctype html>
        <script src="data:text/javascript,d1%20%3D%20'one'%3B"></script>
        <script src="data:text/javascript;base64,ZDIgPSAndHdvJzs%3D"></script>
        <script src="data:text/javascript;base64,%5a%44%4d%67%50%53%41%6e%64%47%68%79%5a%57%55%6e%4f%77%3D%3D"></script>
        <script src="data:text/javascript;base64,%20ZD%20Qg%0D%0APS%20An%20Zm91cic%0D%0A%207%20"></script>
        <script src="data:text/javascript,d5%20%3D%20'five%5Cu0027s'%3B"></script>"#,
    );

    assert_no_errors(&page);
    assert_eq!(
        eval_string(&page, "[d1, d2, d3, d4, d5].join('|')"),
        "one|two|three|four|five's"
    );
}

/// A parser-blocking `data:` script runs before the markup after it is parsed,
/// exactly as an `http:` one would.
#[test]
fn parser_blocking_classic_script_runs_in_order() {
    let page = load(
        r#"<!doctype html>
        <script>window.log = []; log.push('inline-before')</script>
        <script src="data:text/javascript,log.push('external');log.push(document.getElementById('later') === null)"></script>
        <div id="later"></div>
        <script>log.push('inline-after')</script>"#,
    );

    assert_no_errors(&page);
    assert_eq!(
        eval_string(&page, "log.join('|')"),
        "inline-before|external|true|inline-after"
    );
}

/// `defer` keeps document order and runs after the document is parsed.
#[test]
fn deferred_classic_scripts_run_in_document_order_after_parse() {
    let page = load(
        r#"<!doctype html>
        <script defer src="data:text/javascript,log.push('defer-1')"></script>
        <script defer src="data:text/javascript,log.push('defer-2:' + (document.getElementById('later') !== null))"></script>
        <script>window.log = ['inline']</script>
        <div id="later"></div>"#,
    );

    assert_no_errors(&page);
    assert_eq!(
        eval_string(&page, "log.join('|')"),
        "inline|defer-1|defer-2:true"
    );
}

/// An `async` `data:` script must *not* be executed inline by the parser — the
/// property that decoding at the net layer preserves for free and a synchronous
/// inline decode would have broken.
///
/// The assertion is that markup *following* the `<script async>` tag is already
/// in the tree when the script runs. Its order against the later inline script
/// is deliberately not pinned: HTML lets an async script run at any task, so
/// either interleaving is conforming.
#[test]
fn async_classic_script_does_not_block_the_parser() {
    let page = load(
        r#"<!doctype html>
        <script>window.log = []</script>
        <script async src="data:text/javascript,log.push('async-saw-later-markup:' + (document.getElementById('later') !== null))"></script>
        <div id="later"></div>
        <script>log.push('after-async-tag')</script>"#,
    );

    assert_no_errors(&page);
    assert_eq!(
        eval_string(&page, "log.slice().sort().join('|')"),
        "after-async-tag|async-saw-later-markup:true"
    );
}

/// A dynamically inserted `data:` script runs as a task and fires `load`.
#[test]
fn dynamic_script_runs_and_fires_load() {
    let page = load(
        r#"<!doctype html>
        <script>
          window.log = [];
          const s = document.createElement('script');
          s.src = "data:text/javascript,log.push('dynamic')";
          s.onload = () => log.push('load-event');
          s.onerror = () => log.push('error-event');
          document.head.appendChild(s);
          log.push('after-append');
        </script>"#,
    );

    assert_no_errors(&page);
    assert_eq!(
        eval_string(&page, "log.join('|')"),
        "after-append|dynamic|load-event"
    );
}

/// A malformed `data:` URL is a load failure, not a silent success: the element
/// gets `error`, and nothing is evaluated.
#[test]
fn malformed_data_url_script_fires_error() {
    let page = load(
        r#"<!doctype html>
        <script>
          window.log = [];
          const s = document.createElement('script');
          // No comma: the data: URL processor returns failure.
          s.src = "data:text/javascript";
          s.onload = () => log.push('load-event');
          s.onerror = () => log.push('error-event');
          document.head.appendChild(s);
        </script>"#,
    );

    assert_eq!(eval_string(&page, "log.join('|')"), "error-event");
}

/// An external module, and a bare `import` of a second `data:` module from it.
#[test]
fn module_script_and_its_data_url_import() {
    let inner = "data:text/javascript,export const answer = 42;";
    let outer = format!(
        "data:text/javascript,{}",
        percent_encoding::utf8_percent_encode(
            &format!("import {{ answer }} from '{inner}'; window.answer = answer;"),
            percent_encoding::NON_ALPHANUMERIC,
        )
    );
    let page = load(&format!(
        r#"<!doctype html><script type="module" src="{outer}"></script>"#
    ));

    assert_no_errors(&page);
    assert_eq!(eval_string(&page, "String(window.answer)"), "42");
}

/// `<link rel=stylesheet href="data:text/css,...">` applies and fires `load`.
#[test]
fn link_stylesheet_applies_and_fires_load() {
    let page = load(
        r#"<!doctype html>
        <link rel="stylesheet" href="data:text/css,%23t%20%7B%20color%3A%20rgb(1%2C%202%2C%203)%20%7D">
        <div id="t">x</div>"#,
    );

    assert_no_errors(&page);
    assert_eq!(
        eval_string(
            &page,
            "getComputedStyle(document.getElementById('t')).color"
        ),
        "rgb(1, 2, 3)"
    );
}

/// A base64 `data:` stylesheet, and an `@import` of a nested `data:` sheet from
/// it — the blocking `@import` fetcher goes through the same pipeline.
#[test]
fn base64_stylesheet_with_a_data_url_import() {
    // @import url("data:text/css,#u { color: rgb(4, 5, 6) }");
    // #t { color: rgb(7, 8, 9) }
    let sheet = "@import url(\"data:text/css,%23u%20%7B%20color%3A%20rgb(4%2C%205%2C%206)%20%7D\");\
                 #t { color: rgb(7, 8, 9) }";
    let page = load(&format!(
        r#"<!doctype html>
        <link rel="stylesheet" href="data:text/css;base64,{}">
        <div id="t">x</div><div id="u">y</div>"#,
        base64_encode(sheet.as_bytes())
    ));

    assert_no_errors(&page);
    assert_eq!(
        eval_string(
            &page,
            "[getComputedStyle(document.getElementById('t')).color,
              getComputedStyle(document.getElementById('u')).color].join('|')"
        ),
        "rgb(7, 8, 9)|rgb(4, 5, 6)"
    );
}

/// The declared `charset` is honoured for script source, so a non-ASCII body
/// survives. `windows-1251` (not UTF-8) makes the difference observable.
#[test]
fn script_charset_parameter_is_honoured() {
    // "window.s = 'дом'" with the three Cyrillic letters in windows-1251.
    let mut body = b"window.s = '".to_vec();
    body.extend_from_slice(&[0xE4, 0xEE, 0xEC]); // д о м
    body.extend_from_slice(b"'");
    let url = format!(
        "data:text/javascript;charset=windows-1251;base64,{}",
        base64_encode(&body)
    );
    let page = load(&format!(r#"<!doctype html><script src="{url}"></script>"#));

    assert_no_errors(&page);
    assert_eq!(eval_string(&page, "s"), "дом");
}

/// Images and `@font-face` decode `data:` inline rather than through the fetch
/// pipeline, and are handed the whole serialized URL. A fragment on the end must
/// not reach the decoder: `#` is not in the base64 alphabet, so it broke the
/// image outright while the very same URL fetched over `net` decoded fine.
#[test]
fn inline_image_decode_ignores_a_url_fragment() {
    let png = {
        let img = image::RgbaImage::from_pixel(7, 3, image::Rgba([10, 20, 30, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    };
    let page = load(&format!(
        r#"<!doctype html><img id="i" src="data:image/png;base64,{}#frag">"#,
        base64_encode(&png)
    ));

    assert_no_errors(&page);
    assert_eq!(
        eval_string(
            &page,
            "const i = document.getElementById('i');
             [i.naturalWidth, i.naturalHeight].join('x')"
        ),
        "7x3"
    );
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Deciding `data:` in the pipeline rather than at the subresource call sites
/// also makes it reachable by `Page::navigate`, which goes through the same
/// `fetch_blocking`. That is a capability beyond the subresource scope, so it is
/// pinned here rather than left to drift: the document commits, its scripts run,
/// and — because a `data:` URL cannot be a base — every *relative* subresource
/// in it fails to resolve.
#[test]
fn top_level_navigation_to_a_data_url_commits_but_cannot_be_a_base() {
    let page = Page::new(PageOptions::default()).unwrap();
    page.navigate(
        "data:text/html,<h1 id=h>hi</h1><script>window.ran = 1</script>",
        oxidepage_page::WaitUntil::Load,
    )
    .expect("navigate");
    page.settle(Duration::from_secs(2));

    assert_eq!(
        eval_string(&page, "document.getElementById('h').textContent"),
        "hi"
    );
    assert_eq!(eval_string(&page, "String(window.ran)"), "1");
    // The document URL is opaque, so relative resolution has no base to work
    // from. `new URL('a.css', document.URL)` is the resolution the loaders do.
    assert_eq!(
        eval_string(
            &page,
            "(() => { try { new URL('a.css', document.URL); return 'resolved'; }
                      catch (e) { return 'unresolvable'; } })()"
        ),
        "unresolvable"
    );
}
