//! `HTMLElement`'s global-attribute reflectors: `title`, `lang`, `translate`,
//! `hidden`, `inert`, `accessKey`, `autofocus` and `tabIndex`.

use oxidepage_page::{PageOptions, load_html_page};

fn eval(html: &str, expr: &str) -> String {
    let page = load_html_page(html, PageOptions::default()).unwrap();
    page.eval_to_string(expr).expect("eval")
}

const DOC: &str = "<!DOCTYPE html><body><div id=d><span id=s>x</span></div>\
    <details><summary id=first>a</summary><summary id=second>b</summary></details></body>";

#[test]
fn string_and_boolean_reflectors_round_trip() {
    assert_eq!(
        eval(
            DOC,
            "d.title = 't'; d.lang = 'fr'; d.accessKey = 'k'; d.inert = true; d.autofocus = true;\
             [d.getAttribute('title'), d.getAttribute('lang'), d.getAttribute('accesskey'),\
              d.hasAttribute('inert'), d.hasAttribute('autofocus'),\
              (d.inert = false, d.hasAttribute('inert'))].join('|')",
        ),
        "t|fr|k|true|true|false"
    );
}

#[test]
fn translate_inherits_and_reflects_yes_or_no() {
    assert_eq!(
        eval(
            DOC,
            "[s.translate, (d.setAttribute('translate', 'no'), s.translate),\
              (d.setAttribute('translate', 'bogus'), s.translate),\
              (s.translate = false, s.getAttribute('translate'))].join('|')",
        ),
        "true|false|true|no"
    );
}

#[test]
fn hidden_accepts_until_found_and_falsy_values() {
    assert_eq!(
        eval(
            DOC,
            "const out = [d.hidden];\
             d.hidden = 'UNTIL-FOUND'; out.push(d.hidden, d.getAttribute('hidden'));\
             d.hidden = 0; out.push(d.hasAttribute('hidden'));\
             d.hidden = 'x'; out.push(d.hidden, d.getAttribute('hidden'));\
             d.hidden = NaN; out.push(d.hasAttribute('hidden'));\
             d.hidden = {}; out.push(d.hidden);\
             d.hidden = null; out.push(d.hasAttribute('hidden'));\
             out.join('|')",
        ),
        "false|until-found|until-found|false|true||false|true|false"
    );
}

#[test]
fn tab_index_parses_and_defaults_by_element() {
    assert_eq!(
        eval(
            DOC,
            "const t = n => document.createElement(n).tabIndex;\
             [d.tabIndex, t('a'), t('button'), t('iframe'), t('span'),\
              first.tabIndex, second.tabIndex,\
              (d.setAttribute('tabindex', '  7px'), d.tabIndex),\
              (d.setAttribute('tabindex', 'x'), d.tabIndex),\
              (d.tabIndex = -3, d.getAttribute('tabindex'))].join('|')",
        ),
        // Only a `<summary>` that is its `<details>`'s summary defaults to 0.
        "-1|0|0|0|-1|0|-1|7|-1|-3"
    );
}
