//! End-to-end custom-element lifecycle through the real event loop: upgrade of
//! parser-created elements, reaction ordering, connected/disconnected/
//! attributeChanged callbacks, late definition, and `customElements.upgrade`.

use std::time::Duration;

use oxidepage_page::{PageOptions, load_html_page};

fn page(html: &str) -> oxidepage_page::Page {
    load_html_page(html, PageOptions::default()).unwrap()
}

fn s(page: &oxidepage_page::Page, expr: &str) -> String {
    page.eval_to_string(expr).unwrap()
}

#[test]
fn parser_element_is_upgraded_and_connected() {
    let page = page(
        "<!DOCTYPE html><body>\
         <x-foo></x-foo>\
         <script>\
           customElements.define('x-foo', class extends HTMLElement {\
             connectedCallback(){ document.title = 'connected'; }\
           });\
         </script>\
         </body>",
    );
    assert_eq!(s(&page, "document.title"), "connected");
    // The element instance carries the subclass prototype.
    assert_eq!(
        s(
            &page,
            "String(document.querySelector('x-foo') instanceof customElements.get('x-foo'))"
        ),
        "true"
    );
}

#[test]
fn reaction_order_is_ctor_then_attribute_then_connected() {
    let page = page(
        "<!DOCTYPE html><body>\
         <x-ord data-a='1'></x-ord>\
         <script>\
           window.log = [];\
           customElements.define('x-ord', class extends HTMLElement {\
             constructor(){ super(); window.log.push('ctor'); }\
             static get observedAttributes(){ return ['data-a']; }\
             attributeChangedCallback(n,o,v){ window.log.push('attr:'+n+'='+v); }\
             connectedCallback(){ window.log.push('connected'); }\
           });\
         </script>\
         </body>",
    );
    assert_eq!(
        s(&page, "window.log.join(',')"),
        "ctor,attr:data-a=1,connected"
    );
}

#[test]
fn remove_runs_disconnected_callback_before_it_returns() {
    let page = page(
        "<!DOCTYPE html><body>\
         <x-dc id='e'></x-dc>\
         <script>\
           window.log = [];\
           customElements.define('x-dc', class extends HTMLElement {\
             connectedCallback(){ window.log.push('c'); }\
             disconnectedCallback(){ window.log.push('d'); }\
           });\
         </script>\
         </body>",
    );
    assert_eq!(s(&page, "window.log.join(',')"), "c");
    // No settle(): `remove()` is [CEReactions], so the disconnectedCallback has
    // already run by the time the next statement observes the log.
    assert_eq!(
        s(
            &page,
            "(() => { document.getElementById('e').remove(); return window.log.join(','); })()"
        ),
        "c,d"
    );
}

#[test]
fn attribute_changed_only_for_observed() {
    let page = page(
        "<!DOCTYPE html><body>\
         <x-at id='e'></x-at>\
         <script>\
           window.log = [];\
           customElements.define('x-at', class extends HTMLElement {\
             static get observedAttributes(){ return ['watched']; }\
             attributeChangedCallback(n,o,v){ window.log.push(n+'='+v); }\
           });\
         </script>\
         </body>",
    );
    // No settle(): `setAttribute` is [CEReactions], so attributeChangedCallback
    // has already run when the same script reads the log back.
    assert_eq!(
        s(
            &page,
            "(() => {\
               const e = document.getElementById('e');\
               e.setAttribute('watched', 'yes');\
               e.setAttribute('ignored', 'no');\
               return window.log.join(',');\
             })()"
        ),
        "watched=yes"
    );
}

#[test]
fn late_define_upgrades_existing_elements() {
    let page = page(
        "<!DOCTYPE html><body>\
         <x-late></x-late><x-late></x-late>\
         <script>\
           window.n = 0;\
           setTimeout(function(){\
             customElements.define('x-late', class extends HTMLElement {\
               connectedCallback(){ window.n++; }\
             });\
           }, 0);\
         </script>\
         </body>",
    );
    // The deferred define upgrades both existing elements (their
    // connectedCallback runs once each).
    page.settle(Duration::from_millis(100));
    assert_eq!(s(&page, "String(window.n)"), "2");
}

// === `[CEReactions]` timing (ADR-0021) ===
//
// Each of these asserts *within the same script*, with no settle(): the whole
// point is that a `[CEReactions]` operation invokes the reactions it enqueued
// before it returns to script, not one event-loop turn later.

#[test]
fn define_upgrades_parsed_elements_before_it_returns() {
    let page = page(
        "<!DOCTYPE html><body>\
         <x-def></x-def>\
         <script>\
           customElements.define('x-def', class extends HTMLElement {\
             constructor(){ super(); this.upgraded = true; }\
           });\
           window.upgraded = !!document.querySelector('x-def').upgraded;\
         </script>\
         </body>",
    );
    assert_eq!(s(&page, "String(window.upgraded)"), "true");
}

#[test]
fn append_child_runs_connected_callback_before_it_returns() {
    let page = page(
        "<!DOCTYPE html><body>\
         <script>\
           window.log = [];\
           customElements.define('x-ap', class extends HTMLElement {\
             connectedCallback(){ window.log.push('connected'); }\
           });\
           const el = document.createElement('x-ap');\
           window.log.push('before');\
           document.body.appendChild(el);\
           window.log.push('after');\
         </script>\
         </body>",
    );
    assert_eq!(s(&page, "window.log.join(',')"), "before,connected,after");
}

#[test]
fn inner_html_upgrades_before_the_setter_returns() {
    // The ordinary web-component idiom: set markup, then immediately reach into
    // it and call a method on the new element.
    let page = page(
        "<!DOCTYPE html><body>\
         <div id='c'></div>\
         <script>\
           customElements.define('x-ih', class extends HTMLElement {\
             method(){ return 'ok'; }\
           });\
           const c = document.getElementById('c');\
           c.innerHTML = '<x-ih></x-ih>';\
           window.result = c.querySelector('x-ih').method();\
         </script>\
         </body>",
    );
    assert_eq!(s(&page, "window.result"), "ok");
}

#[test]
fn a_reaction_that_mutates_the_dom_nests() {
    // A connectedCallback that appends another custom element: the inner
    // element's own reactions run to completion inside the nested appendChild,
    // before the outer operation returns.
    let page = page(
        "<!DOCTYPE html><body>\
         <script>\
           window.log = [];\
           customElements.define('x-inner', class extends HTMLElement {\
             connectedCallback(){ window.log.push('inner'); }\
           });\
           customElements.define('x-outer', class extends HTMLElement {\
             connectedCallback(){\
               window.log.push('outer-start');\
               this.appendChild(document.createElement('x-inner'));\
               window.log.push('outer-end:' + window.log.includes('inner'));\
             }\
           });\
           document.body.appendChild(document.createElement('x-outer'));\
           window.log.push('after');\
         </script>\
         </body>",
    );
    assert_eq!(
        s(&page, "window.log.join(',')"),
        "outer-start,inner,outer-end:true,after"
    );
}

#[test]
fn a_detached_fragment_is_not_upgraded() {
    // Fragment parsing happens in a browsing-context-less document, whose
    // registry is empty: `innerHTML =` on a *detached* host creates the element
    // but never upgrades it. Upgrading is the insertion steps' job, and they
    // only run it when the parent is connected (ADR-0021 §6).
    let page = page(
        "<!DOCTYPE html><body>\
         <script>\
           customElements.define('x-frag', class extends HTMLElement {\
             constructor(){ super(); this.upgraded = true; }\
           });\
           window.host = document.createElement('div');\
           window.host.innerHTML = '<x-frag></x-frag>';\
           window.detached = !!window.host.firstChild.upgraded;\
         </script>\
         </body>",
    );
    assert_eq!(s(&page, "String(window.detached)"), "false");
    // …and it stays un-upgraded: this is not merely deferred work.
    page.settle(Duration::from_millis(50));
    assert_eq!(
        s(&page, "String(!!window.host.firstChild.upgraded)"),
        "false"
    );
    // Connecting the host upgrades it, synchronously.
    assert_eq!(
        s(
            &page,
            "(() => {\
               document.body.appendChild(window.host);\
               return String(!!window.host.firstChild.upgraded);\
             })()"
        ),
        "true"
    );
}

#[test]
fn explicit_upgrade_is_synchronous() {
    let page = page(
        "<!DOCTYPE html><body>\
         <script>\
           customElements.define('x-up', class extends HTMLElement {\
             constructor(){ super(); this.upgraded = true; }\
           });\
           const host = document.createElement('div');\
           host.innerHTML = '<x-up></x-up>';\
           const el = host.firstChild;\
           window.beforeInst = el instanceof customElements.get('x-up');\
           customElements.upgrade(host);\
           window.afterInst = el instanceof customElements.get('x-up');\
         </script>\
         </body>",
    );
    assert_eq!(s(&page, "String(window.beforeInst)"), "false");
    assert_eq!(s(&page, "String(window.afterInst)"), "true");
}

#[test]
fn create_element_upgrades_synchronously() {
    let page = page(
        "<!DOCTYPE html><body>\
         <script>\
           customElements.define('x-ce', class extends HTMLElement {\
             constructor(){ super(); this.made = 42; }\
           });\
           const el = document.createElement('x-ce');\
           window.made = el.made;\
           window.inst = el instanceof customElements.get('x-ce');\
         </script>\
         </body>",
    );
    assert_eq!(s(&page, "String(window.made)"), "42");
    assert_eq!(s(&page, "String(window.inst)"), "true");
}

#[test]
fn when_defined_resolves() {
    let page = page(
        "<!DOCTYPE html><body>\
         <script>\
           window.resolved = false;\
           customElements.whenDefined('x-wd').then(() => { window.resolved = true; });\
           setTimeout(function(){\
             customElements.define('x-wd', class extends HTMLElement {});\
           }, 0);\
         </script>\
         </body>",
    );
    page.settle(Duration::from_millis(100));
    assert_eq!(s(&page, "String(window.resolved)"), "true");
}
