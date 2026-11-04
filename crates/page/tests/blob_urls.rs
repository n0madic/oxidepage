//! `blob:` URLs (ADR-0042): `URL.createObjectURL`, the page's blob URL
//! store, and every consumer resolving through it.

use std::time::Duration;

use oxidepage_page::{Page, PageOptions, load_html_page};

const ORIGIN: &str = "https://a.test";

fn page_with(html: &str) -> Page {
    load_html_page(
        html,
        PageOptions {
            url: Some(format!("{ORIGIN}/index.html")),
            ..PageOptions::default()
        },
    )
    .expect("page")
}

fn eval(page: &Page, expr: &str) -> String {
    page.eval_to_string(expr).expect("eval")
}

/// Runs `script`, which must eventually set `document.title`, and reads it.
fn settled_title(page: &Page, script: &str) -> String {
    page.eval_to_string(&format!("{script}; 0")).expect("eval");
    page.settle(Duration::from_millis(1000));
    eval(page, "document.title")
}

#[test]
fn create_object_url_mints_an_origin_scoped_uuid_url() {
    let page = page_with("<!doctype html>");
    assert_eq!(
        eval(
            &page,
            "const u = URL.createObjectURL(new Blob(['x']));\
             const v = URL.createObjectURL(new Blob(['x']));\
             [/^blob:https:\\/\\/a\\.test\\/[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(u),\
              u !== v, new URL(u).origin].join()",
        ),
        "true,true,https://a.test"
    );
    assert_eq!(
        eval(
            &page,
            "const name = (f) => { try { f(); return 'ok'; } catch (e) { return e.name; } };\
             const d = Object.getOwnPropertyDescriptor(URL, 'createObjectURL');\
             [name(() => URL.createObjectURL('text')), name(() => URL.createObjectURL()),\
              URL.createObjectURL.length, URL.revokeObjectURL.length, d.enumerable, d.writable,\
              typeof __oxide_createObjectURL].join()",
        ),
        "TypeError,TypeError,1,1,true,true,undefined"
    );
}

/// `fetch` and XHR read the blob's bytes and type; a revoked URL, a non-GET
/// method and an unknown URL are network errors.
#[test]
fn fetch_and_xhr_resolve_a_blob_url() {
    let page = page_with("<!doctype html>");
    let title = settled_title(
        &page,
        "const u = URL.createObjectURL(new Blob(['héllo'], { type: 'text/plain' }));\
         const parts = [];\
         fetch(u + '#ignored')\
           .then((r) => { parts.push(r.status, r.headers.get('content-type'), r.headers.get('content-length')); return r.text(); })\
           .then((t) => { parts.push(t); return fetch(u, { method: 'POST' }).catch((e) => parts.push(e.name)); })\
           .then(() => new Promise((done) => {\
              const x = new XMLHttpRequest(); x.open('GET', u);\
              x.onload = () => { parts.push('xhr:' + x.responseText); done(); }; x.send();\
           }))\
           .then(() => { URL.revokeObjectURL(u); return fetch(u).catch((e) => parts.push('revoked:' + e.name)); })\
           .then(() => fetch('blob:https://a.test/nope').catch((e) => parts.push('unknown:' + e.name)))\
           .then(() => { document.title = parts.join('|'); })",
    );
    assert_eq!(
        title,
        "200|text/plain|6|héllo|TypeError|xhr:héllo|revoked:TypeError|unknown:TypeError"
    );
}

/// A script and a frame load from blob URLs, and the frame's document has the
/// blob's origin — the page's — so the embedder can reach into it.
#[test]
fn scripts_and_frames_load_from_blob_urls() {
    let page = page_with("<!doctype html><body></body>");
    let title = settled_title(
        &page,
        "const js = URL.createObjectURL(new Blob(['document.title += \"script;\"'], { type: 'text/javascript' }));\
         const s = document.createElement('script'); s.src = js; document.body.append(s);\
         const html = URL.createObjectURL(new Blob(['<p id=p>framed</p>'], { type: 'text/html' }));\
         const f = document.createElement('iframe');\
         f.onload = () => { document.title += f.contentDocument.getElementById('p').textContent; };\
         f.src = html; document.body.append(f)",
    );
    // Two independent loads, so either may finish first.
    assert!(
        title == "script;framed" || title == "framedscript;",
        "{title}"
    );
}

/// Revocation follows the File API: only the creating origin can revoke, and
/// the creator's document unloading revokes everything it made.
#[test]
fn only_the_origin_revokes_and_unloading_revokes_everything() {
    let page = page_with("<!doctype html>");
    let url = eval(&page, "URL.createObjectURL(new Blob(['still here']))");
    // A URL from another origin is ignored, even if it named this entry's id.
    let spoof = url.replace(ORIGIN, "https://evil.test");
    let title = settled_title(
        &page,
        &format!(
            "URL.revokeObjectURL('{spoof}'); URL.revokeObjectURL('not a url');\
             fetch('{url}').then((r) => r.text()).then((t) => {{ document.title = t; }})"
        ),
    );
    assert_eq!(title, "still here");

    page.load_html("<!doctype html><title>next</title>")
        .expect("load");
    let title = settled_title(
        &page,
        &format!(
            "fetch('{url}').then(() => {{ document.title = 'alive'; }}, (e) => {{ document.title = e.name; }})"
        ),
    );
    assert_eq!(
        title, "TypeError",
        "the outgoing document's URL died with it"
    );
}
