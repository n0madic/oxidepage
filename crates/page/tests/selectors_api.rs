//! The Selectors API surface: `matches`/`closest`/`querySelector*`, the
//! `webkitMatchesSelector` alias, WebIDL's required-argument check, and
//! `:lang()` both in selector matching and in the cascade.

use oxidepage_page::{PageOptions, load_html_page};

fn eval(html: &str, expr: &str) -> String {
    let page = load_html_page(html, PageOptions::default()).unwrap();
    page.eval_to_string(expr).expect("eval")
}

const LANG: &str = "<!DOCTYPE html><html lang=en-US><body>\
    <div id=fr lang=fr><span id=inner>x</span></div>\
    <div id=empty lang=''><span id=unknown>y</span></div>\
    <svg id=svg xml:lang=ja><g id=g /></svg>\
    <div id=host></div></body></html>";

#[test]
fn webkit_matches_selector_is_an_alias_of_matches() {
    assert_eq!(
        eval(
            LANG,
            "[typeof Element.prototype.webkitMatchesSelector,\
              fr.webkitMatchesSelector('div[lang]'), fr.webkitMatchesSelector('span'),\
              Element.prototype.webkitMatchesSelector.length].join('|')",
        ),
        "function|true|false|1"
    );
}

#[test]
fn a_missing_required_argument_is_a_type_error() {
    // Before the check, a missing `DOMString` was the string "undefined":
    // `matches()` answered false and `querySelector()` looked for <undefined>.
    assert_eq!(
        eval(
            LANG,
            "const kind = f => { try { f(); return 'ok'; } catch (e) { return e.constructor.name; } };\
             [kind(() => fr.matches()), kind(() => fr.closest()),\
              kind(() => document.querySelector()), kind(() => document.createElement()),\
              kind(() => fr.matches(undefined)), kind(() => new CustomEvent())].join('|')",
        ),
        "TypeError|TypeError|TypeError|TypeError|ok|TypeError"
    );
}

#[test]
fn lang_matches_the_nearest_declared_language() {
    assert_eq!(
        eval(
            LANG,
            "[fr.matches(':lang(fr)'), inner.matches(':lang(fr)'), inner.matches(':lang(en)'),\
              document.body.matches(':lang(en)'), document.body.matches(':lang(en-US)'),\
              document.body.matches(':lang(en-GB)'), g.matches(':lang(ja)'),\
              unknown.matches(':lang(en)'),\
              document.querySelectorAll(':lang(fr)').length].join('|')",
        ),
        // `lang=""` declares the language unknown, which stops inheritance.
        "true|true|false|true|true|false|true|false|2"
    );
}

#[test]
fn lang_crosses_a_shadow_root_to_its_host() {
    assert_eq!(
        eval(
            LANG,
            "host.lang = 'de';\
             const root = host.attachShadow({mode: 'open'});\
             root.innerHTML = '<p id=p>z</p>';\
             root.getElementById('p').matches(':lang(de)')",
        ),
        "true"
    );
}

#[test]
fn lang_drives_the_cascade_and_restyles_on_change() {
    let html = "<!DOCTYPE html><html lang=en><style>\
        :lang(fr) { color: rgb(1, 2, 3) }\
        p:lang(de) span { color: rgb(4, 5, 6) }</style>\
        <body><div lang=fr><span id=a>a</span></div><p id=p><span id=b>b</span></p></body></html>";
    assert_eq!(
        eval(
            html,
            "[getComputedStyle(a).color, getComputedStyle(b).color,\
              (p.lang = 'de', getComputedStyle(b).color),\
              (p.removeAttribute('lang'), getComputedStyle(b).color)].join('|')",
        ),
        "rgb(1, 2, 3)|rgb(0, 0, 0)|rgb(4, 5, 6)|rgb(0, 0, 0)"
    );
}
