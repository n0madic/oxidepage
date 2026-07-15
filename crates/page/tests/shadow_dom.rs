//! End-to-end Shadow DOM through the real event loop: attachShadow + slot
//! projection driving layout, shadow-scoped styles (`:host`, `::slotted`,
//! `::part`), adoptedStyleSheets, and custom elements attaching shadow in
//! their constructor.

use oxidepage_page::{PageOptions, load_html_page};

fn page(html: &str) -> oxidepage_page::Page {
    load_html_page(html, PageOptions::default()).unwrap()
}

fn s(page: &oxidepage_page::Page, expr: &str) -> String {
    page.eval_to_string(expr).unwrap()
}

#[test]
fn slotted_content_participates_in_layout() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <div id='host'><p id='light' style='height:30px;margin:0'>light</p></div>\
         <script>\
           const sr = document.getElementById('host').attachShadow({mode:'open'});\
           sr.innerHTML = '<div id=\"frame\" style=\"height:20px\"></div><slot></slot>';\
         </script>\
         </body>",
    );
    // The slotted light paragraph flows after the 20px shadow div.
    assert_eq!(s(&page, "document.getElementById('light').offsetTop"), "20");
    assert_eq!(
        s(&page, "document.getElementById('light').offsetHeight"),
        "30"
    );
    // The host's height covers its flat-tree (shadow) contents.
    assert_eq!(
        s(&page, "document.getElementById('host').offsetHeight"),
        "50"
    );
}

#[test]
fn unassigned_light_children_do_not_render() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <div id='host'>\
           <p id='lost' slot='nope' style='height:40px;margin:0'>lost</p>\
           <p id='kept' style='height:10px;margin:0'>kept</p>\
         </div>\
         <script>\
           const sr = document.getElementById('host').attachShadow({mode:'open'});\
           sr.innerHTML = '<slot></slot>';\
         </script>\
         </body>",
    );
    assert_eq!(
        s(&page, "document.getElementById('host').offsetHeight"),
        "10"
    );
    assert_eq!(
        s(&page, "document.getElementById('lost').offsetHeight"),
        "0"
    );
}

#[test]
fn slot_fallback_renders_when_nothing_assigned() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <div id='host'></div>\
         <script>\
           const sr = document.getElementById('host').attachShadow({mode:'open'});\
           sr.innerHTML = '<slot><em style=\"display:block;height:25px\">fallback</em></slot>';\
         </script>\
         </body>",
    );
    assert_eq!(
        s(&page, "document.getElementById('host').offsetHeight"),
        "25"
    );
}

#[test]
fn shadow_styles_are_scoped() {
    let page = page(
        "<!DOCTYPE html><head><style>p { color: rgb(0, 0, 255); }</style></head>\
         <body>\
         <div id='host'></div><p id='doc'>doc</p>\
         <script>\
           const sr = document.getElementById('host').attachShadow({mode:'open'});\
           sr.innerHTML = '<style>p { color: rgb(255, 0, 0); }</style><p id=\"inner\">in</p>';\
           window.inner = sr.getElementById('inner');\
         </script>\
         </body>",
    );
    // Shadow <style> applies inside the shadow tree…
    assert_eq!(
        s(&page, "getComputedStyle(window.inner).color"),
        "rgb(255, 0, 0)"
    );
    // …and does not leak into the document; document styles do not leak in.
    assert_eq!(
        s(
            &page,
            "getComputedStyle(document.getElementById('doc')).color"
        ),
        "rgb(0, 0, 255)"
    );
}

#[test]
fn host_and_slotted_selectors_apply() {
    let page = page(
        "<!DOCTYPE html><body>\
         <div id='host'><span id='light'>x</span></div>\
         <script>\
           const sr = document.getElementById('host').attachShadow({mode:'open'});\
           sr.innerHTML = '<style>\
             :host { display: block; padding-left: 7px; }\
             ::slotted(span) { color: rgb(0, 128, 0); }\
           </style><slot></slot>';\
         </script>\
         </body>",
    );
    assert_eq!(
        s(
            &page,
            "getComputedStyle(document.getElementById('host')).paddingLeft"
        ),
        "7px"
    );
    assert_eq!(
        s(
            &page,
            "getComputedStyle(document.getElementById('light')).color"
        ),
        "rgb(0, 128, 0)"
    );
}

#[test]
fn part_selector_styles_shadow_element_from_document() {
    let page = page(
        "<!DOCTYPE html><head><style>#host::part(label) { color: rgb(200, 0, 0); }</style></head>\
         <body>\
         <div id='host'></div>\
         <script>\
           const sr = document.getElementById('host').attachShadow({mode:'open'});\
           sr.innerHTML = '<span part=\"label\" id=\"inner\">x</span>';\
           window.inner = sr.getElementById('inner');\
         </script>\
         </body>",
    );
    assert_eq!(
        s(&page, "getComputedStyle(window.inner).color"),
        "rgb(200, 0, 0)"
    );
}

#[test]
fn slotted_content_inherits_through_the_slot() {
    let page = page(
        "<!DOCTYPE html><body>\
         <div id='host'><span id='light'>x</span></div>\
         <script>\
           const sr = document.getElementById('host').attachShadow({mode:'open'});\
           sr.innerHTML = '<style>div { color: rgb(1, 2, 3); }</style><div><slot></slot></div>';\
         </script>\
         </body>",
    );
    // The light span inherits color from the shadow <div> it is slotted
    // into (flat-tree inheritance), not from the host.
    assert_eq!(
        s(
            &page,
            "getComputedStyle(document.getElementById('light')).color"
        ),
        "rgb(1, 2, 3)"
    );
}

#[test]
fn adopted_style_sheets_apply_in_shadow() {
    let page = page(
        "<!DOCTYPE html><body>\
         <div id='host'></div>\
         <script>\
           const sr = document.getElementById('host').attachShadow({mode:'open'});\
           sr.innerHTML = '<p id=\"inner\">x</p>';\
           const sheet = new CSSStyleSheet();\
           sheet.replaceSync('p { color: rgb(9, 9, 9); }');\
           sr.adoptedStyleSheets = [sheet];\
           window.inner = sr.getElementById('inner');\
         </script>\
         </body>",
    );
    assert_eq!(
        s(&page, "getComputedStyle(window.inner).color"),
        "rgb(9, 9, 9)"
    );
    // replaceSync after adoption re-styles adopters.
    page.eval_to_string(
        "document.querySelector('#host').shadowRoot.adoptedStyleSheets[0]\
         .replaceSync('p { color: rgb(4, 5, 6); }'); 'ok'",
    )
    .unwrap();
    assert_eq!(
        s(&page, "getComputedStyle(window.inner).color"),
        "rgb(4, 5, 6)"
    );
}

#[test]
fn adopted_style_sheets_push_applies() {
    // ObservableArray semantics: `push` (not only reassignment) must reach
    // the style engine.
    let page = page(
        "<!DOCTYPE html><body>\
         <div id='host'></div>\
         <script>\
           const sr = document.getElementById('host').attachShadow({mode:'open'});\
           sr.innerHTML = '<p id=\"inner\">x</p>';\
           const sheet = new CSSStyleSheet();\
           sheet.replaceSync('p { color: rgb(7, 8, 9); }');\
           sr.adoptedStyleSheets.push(sheet);\
           window.inner = sr.getElementById('inner');\
         </script>\
         </body>",
    );
    assert_eq!(
        s(&page, "getComputedStyle(window.inner).color"),
        "rgb(7, 8, 9)"
    );
}

/// Regression: `adoptedStyleSheets` may list the same sheet twice — the array
/// is a plain list, and duplicates are legal. Handing stylo the same sheet
/// twice trips an assertion in its sheet set and takes the process down, so
/// duplicates are dropped; the survivor keeps the *last* position, which is the
/// cascade order the CSSOM gives it (`[b, a, b]` cascades as `[a, b]`).
#[test]
fn duplicate_adopted_style_sheets_cascade_at_their_last_position() {
    let page = page(
        "<!DOCTYPE html><body>\
         <p id='t'>x</p>\
         <script>\
           const a = new CSSStyleSheet();\
           a.replaceSync('p { color: rgb(1, 1, 1); }');\
           const b = new CSSStyleSheet();\
           b.replaceSync('p { color: rgb(2, 2, 2); }');\
           document.adoptedStyleSheets = [b, a, b];\
         </script>\
         </body>",
    );
    // `b` last → `b` wins, even though `a` sits after `b`'s first mention.
    assert_eq!(
        s(
            &page,
            "getComputedStyle(document.getElementById('t')).color"
        ),
        "rgb(2, 2, 2)"
    );
}

/// The same duplicate, on a shadow scope: a separate code path
/// (`flush_shadow_scopes`) appends the sheets, and it hits the same assertion.
#[test]
fn duplicate_adopted_style_sheets_in_shadow_do_not_panic() {
    let page = page(
        "<!DOCTYPE html><body>\
         <div id='host'></div>\
         <script>\
           const sr = document.getElementById('host').attachShadow({mode:'open'});\
           sr.innerHTML = '<p id=\"inner\">x</p>';\
           const a = new CSSStyleSheet();\
           a.replaceSync('p { color: rgb(1, 1, 1); }');\
           const b = new CSSStyleSheet();\
           b.replaceSync('p { color: rgb(2, 2, 2); }');\
           sr.adoptedStyleSheets = [b, a, b];\
           window.inner = sr.getElementById('inner');\
         </script>\
         </body>",
    );
    assert_eq!(
        s(&page, "getComputedStyle(window.inner).color"),
        "rgb(2, 2, 2)"
    );
}

#[test]
fn custom_element_attaches_shadow_in_constructor() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <x-card><span slot='title'>T</span></x-card>\
         <script>\
           customElements.define('x-card', class extends HTMLElement {\
             constructor() {\
               super();\
               const sr = this.attachShadow({mode:'open'});\
               sr.innerHTML = '<style>:host{display:block} .t{height:15px}</style>\
                 <div class=\"t\"><slot name=\"title\"></slot></div>';\
             }\
           });\
           window.card = document.querySelector('x-card');\
         </script>\
         </body>",
    );
    assert_eq!(s(&page, "String(window.card.shadowRoot !== null)"), "true");
    assert_eq!(s(&page, "window.card.offsetHeight"), "15");
    assert_eq!(
        s(&page, "window.card.querySelector('span').assignedSlot.name"),
        "title"
    );
}

#[test]
fn shadow_mutation_after_load_relayouts() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <div id='host'></div>\
         <script>\
           const sr = document.getElementById('host').attachShadow({mode:'open'});\
           sr.innerHTML = '<div id=\"grow\" style=\"height:10px\"></div>';\
         </script>\
         </body>",
    );
    assert_eq!(
        s(&page, "document.getElementById('host').offsetHeight"),
        "10"
    );
    // A style mutation inside the shadow tree must invalidate layout
    // (try_patch bails while shadow roots exist).
    assert_eq!(
        s(
            &page,
            "document.getElementById('host').shadowRoot.getElementById('grow')\
             .style.height = '60px'; document.getElementById('host').offsetHeight"
        ),
        "60"
    );
}
