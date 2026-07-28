//! End-to-end bindings tests: JS scripts against a parsed document,
//! without the page event loop (timers are stubbed).

use std::cell::RefCell;
use std::rc::Rc;

use oxidepage_base::{RequestId, id::FIRST_GENERATION};
use oxidepage_bindings::{
    BindCx, ConsoleLevel, ConsoleMessage, DialogEvent, DialogRequest, DialogResponse, HostHooks,
    PageState, ScriptError, install,
};
use oxidepage_bindings::{PrivateStorageAreas, SharedStorage, StorageAreaKind};
use oxidepage_dom::{ParseOptions, parse_document};
use oxidepage_js::{JsEngine, JsRealm, JsValue, QuickJsEngine, RealmOptions};
use oxidepage_net::{CookieJar, NetEvent, NetRequest, ResponseType};

/// Test hooks with a real cookie jar (no network); fetch/XHR are not started.
struct TestHooks {
    console: RefCell<Vec<ConsoleMessage>>,
    errors: RefCell<Vec<ScriptError>>,
    cookies: RefCell<CookieJar>,
    next_id: std::cell::Cell<u32>,
    /// Answers handed to `alert`/`confirm`/`prompt`, oldest first; the
    /// auto-dismiss default applies once it runs out.
    dialog_answers: RefCell<std::collections::VecDeque<DialogResponse>>,
    dialogs: RefCell<Vec<DialogEvent>>,
    storage: PrivateStorageAreas,
}

impl Default for TestHooks {
    fn default() -> Self {
        Self {
            console: RefCell::new(Vec::new()),
            errors: RefCell::new(Vec::new()),
            cookies: RefCell::new(CookieJar::new()),
            next_id: std::cell::Cell::new(1),
            dialog_answers: RefCell::new(std::collections::VecDeque::new()),
            dialogs: RefCell::new(Vec::new()),
            storage: PrivateStorageAreas::default(),
        }
    }
}

impl HostHooks for TestHooks {
    /// One area per (kind, origin), private to this test page — the standalone
    /// behavior, with no browsing context to share with.
    fn storage(&self, kind: StorageAreaKind, origin: &str) -> SharedStorage {
        self.storage.area(kind, origin)
    }

    fn console_message(&self, message: ConsoleMessage) {
        self.console.borrow_mut().push(message);
    }

    fn report_error(&self, error: ScriptError) {
        self.errors.borrow_mut().push(error);
    }

    fn run_dialog(&self, request: DialogRequest) -> DialogResponse {
        let response = self
            .dialog_answers
            .borrow_mut()
            .pop_front()
            .unwrap_or_default();
        self.dialogs.borrow_mut().push(DialogEvent {
            kind: request.kind,
            message: request.message,
            default_value: request.default_value,
            response: response.clone(),
            timestamp: 0.0,
        });
        response
    }

    fn schedule_timer(
        &self,
        _callback: JsValue,
        _args: Vec<JsValue>,
        _delay_ms: f64,
        _repeat: bool,
    ) -> f64 {
        0.0
    }

    fn clear_timer(&self, _id: f64) {}

    fn request_animation_frame(&self, _callback: JsValue) -> f64 {
        0.0
    }

    fn cancel_animation_frame(&self, _id: f64) {}

    fn start_fetch(&self, _request: NetRequest) -> RequestId {
        // No network in bindings tests; hand back a unique id.
        let n = self.next_id.get();
        self.next_id.set(n + 1);
        RequestId::from_parts(n, FIRST_GENERATION)
    }

    fn abort(&self, _id: RequestId) {}

    fn get_cookie(&self, document_url: &str) -> String {
        let Ok(url) = url::Url::parse(document_url) else {
            return String::new();
        };
        self.cookies
            .borrow_mut()
            .document_cookie(&url, std::time::SystemTime::now())
    }

    fn set_cookie(&self, document_url: &str, cookie: &str) {
        if let Ok(url) = url::Url::parse(document_url) {
            self.cookies.borrow_mut().set_document_cookie(
                &url,
                cookie,
                std::time::SystemTime::now(),
            );
        }
    }
}

struct Harness {
    // Field order = drop order: the state (which owns persistent JS
    // references) must drop before the realm.
    state: Rc<PageState>,
    hooks: Rc<TestHooks>,
    realm: oxidepage_js::QuickJsRealm,
}

impl Harness {
    fn new(html: &str) -> Self {
        let realm = QuickJsEngine
            .new_realm(RealmOptions::default())
            .expect("realm");
        let dom = Rc::new(RefCell::new(
            parse_document(html, ParseOptions::default()).tree,
        ));
        let hooks = Rc::new(TestHooks::default());
        let state = install(
            &realm,
            dom,
            Rc::clone(&hooks) as Rc<dyn HostHooks>,
            oxidepage_style::Viewport::default(),
        )
        .expect("install");
        Self {
            state,
            hooks,
            realm,
        }
    }

    fn eval(&self, source: &str) -> Result<JsValue, oxidepage_js::JsError> {
        self.realm.with_scope(|scope| {
            let result = scope.eval(source, "test.js");
            let cx = BindCx {
                scope,
                state: Rc::clone(&self.state),
            };
            oxidepage_bindings::microtask_checkpoint(&cx);
            result
        })
    }

    fn eval_string(&self, source: &str) -> String {
        match self.eval(source) {
            Ok(JsValue::String(s)) => s,
            Ok(other) => panic!("expected string, got {other:?}"),
            Err(e) => panic!("eval failed: {e}"),
        }
    }

    fn eval_bool(&self, source: &str) -> bool {
        match self.eval(source) {
            Ok(JsValue::Bool(b)) => b,
            Ok(other) => panic!("expected bool, got {other:?}"),
            Err(e) => panic!("eval failed: {e}"),
        }
    }

    fn eval_number(&self, source: &str) -> f64 {
        match self.eval(source) {
            Ok(JsValue::Number(n)) => n,
            Ok(other) => panic!("expected number, got {other:?}"),
            Err(e) => panic!("eval failed: {e}"),
        }
    }
}

const PAGE: &str = r#"<!DOCTYPE html>
<html><head><title>Test Page</title></head>
<body>
  <div id="main" class="box big">
    <p class="para">first</p>
    <p class="para">second</p>
    <span data-x="1">span text</span>
  </div>
</body></html>"#;

#[test]
fn document_basics() {
    let h = Harness::new(PAGE);
    assert_eq!(h.eval_string("document.title"), "Test Page");
    assert_eq!(h.eval_string("document.nodeName"), "#document");
    assert_eq!(h.eval_number("document.nodeType"), 9.0);
    assert_eq!(h.eval_string("document.documentElement.tagName"), "HTML");
    assert_eq!(h.eval_string("document.body.tagName"), "BODY");
    assert_eq!(h.eval_string("document.compatMode"), "CSS1Compat");
    assert!(h.eval_bool("document === window.document"));
    assert!(h.eval_bool("window === self && window === globalThis"));
    assert!(h.eval_bool("document instanceof Document && document instanceof Node"));
    assert!(h.eval_bool("document instanceof EventTarget"));
}

// === Window named access ===

const NAMED: &str = r#"<!DOCTYPE html><html><body>
  <div id="myDiv"></div>
  <div id="location"></div>
  <div id="document"></div>
  <span id="dup">first</span>
  <span id="dup">second</span>
</body></html>"#;

/// Evaluates `source` and reports the constructor name of whatever it threw,
/// or `"NO THROW"`.
fn threw(h: &Harness, source: &str) -> String {
    h.eval_string(&format!(
        "(() => {{ try {{ {source}; return 'NO THROW'; }} catch (e) {{ return e.constructor.name; }} }})()"
    ))
}

#[test]
fn element_ids_are_named_properties_of_the_window() {
    let h = Harness::new(NAMED);
    assert!(h.eval_bool("window.myDiv === document.getElementById('myDiv')"));
    assert!(h.eval_bool("myDiv === document.getElementById('myDiv')"));
    assert!(h.eval_bool("myDiv.tagName === 'DIV'"));
    // Non-enumerable, per WebIDL.
    assert!(h.eval_bool("!Object.keys(window).includes('myDiv')"));
}

/// The named properties object must not turn undeclared identifiers into
/// `undefined`. (It is a plain object precisely because a `Proxy` in the
/// window's prototype chain does exactly that under QuickJS.)
#[test]
fn an_absent_name_still_throws_reference_error() {
    let h = Harness::new(NAMED);
    assert!(h.eval_bool("typeof totallyAbsent === 'undefined'"));
    assert!(h.eval_bool("window.totallyAbsent === undefined"));
    assert_eq!(threw(&h, "void totallyAbsent"), "ReferenceError");
}

/// Own properties of the window are found before its prototype chain, so an
/// element id can never shadow one.
#[test]
fn window_own_properties_shadow_element_ids() {
    let h = Harness::new(NAMED);
    assert!(h.eval_bool("window.location !== document.getElementById('location')"));
    assert!(h.eval_bool("typeof location.href === 'string'"));
    assert!(h.eval_bool("window.document !== document.getElementById('document')"));
    assert!(h.eval_bool("document.nodeType === 9"));
}

/// The named properties object answers `[[GetOwnProperty]]` with a data
/// descriptor in the spec, so assigning through it creates an own property on
/// the window; deleting that property uncovers the element again.
#[test]
fn assigning_a_named_property_shadows_the_element() {
    let h = Harness::new(NAMED);
    assert!(h.eval_bool(
        "window.myDiv = 99; window.hasOwnProperty('myDiv') && window.myDiv === 99 && myDiv === 99"
    ));
    assert!(h.eval_bool("delete window.myDiv; window.myDiv === document.getElementById('myDiv')"));
}

#[test]
fn duplicate_ids_name_the_first_element_in_tree_order() {
    let h = Harness::new(NAMED);
    assert!(h.eval_bool("window.dup === document.getElementById('dup')"));
    assert_eq!(h.eval_string("dup.textContent"), "first");
}

/// Names appear and disappear with the elements, within a single script.
#[test]
fn named_properties_track_dom_mutations() {
    let h = Harness::new(NAMED);
    assert!(h.eval_bool(
        "const d = document.createElement('div');
         d.id = 'zz';
         document.body.appendChild(d);
         typeof zz === 'object' && zz === d"
    ));
    assert_eq!(threw(&h, "void nope"), "ReferenceError");

    assert!(h.eval_bool("document.body.removeChild(document.getElementById('zz')); true"));
    assert_eq!(threw(&h, "void zz"), "ReferenceError");
    assert!(h.eval_bool("window.zz === undefined"));
}

/// The index only tracks connected elements, so `getElementById` on a detached
/// `DocumentFragment` keeps using the subtree walk.
#[test]
fn get_element_by_id_works_on_a_document_fragment() {
    let h = Harness::new(NAMED);
    assert!(h.eval_bool(
        "const f = document.createDocumentFragment();
         const d = document.createElement('div');
         d.id = 'inFragment';
         f.appendChild(d);
         f.getElementById('inFragment') === d
             && document.getElementById('inFragment') === null
             && window.inFragment === undefined"
    ));
}

/// The named properties object sits between `Window.prototype` and
/// `EventTarget.prototype`, invisibly to the documented chain assertions.
#[test]
fn named_properties_object_keeps_the_window_prototype_chain() {
    let h = Harness::new(NAMED);
    assert!(h.eval_bool("Object.getPrototypeOf(window) === Window.prototype"));
    assert!(h.eval_bool("Window.prototype instanceof EventTarget"));
    assert!(h.eval_bool("window instanceof Window && window instanceof EventTarget"));
    assert!(h.eval_bool("window.addEventListener === EventTarget.prototype.addEventListener"));
}

/// A root interface's prototype object inherits `Object.prototype` (WebIDL),
/// so `Object.prototype`'s methods reach every interface — including the
/// window, whose chain runs through `EventTarget.prototype`.
#[test]
fn root_interface_prototypes_inherit_object_prototype() {
    let h = Harness::new(PAGE);
    assert!(h.eval_bool("Object.getPrototypeOf(EventTarget.prototype) === Object.prototype"));
    assert!(h.eval_bool("Object.getPrototypeOf(Navigator.prototype) === Object.prototype"));
    assert!(h.eval_bool("window instanceof Object"));
    assert!(h.eval_bool("window.hasOwnProperty('document')"));
    assert!(h.eval_bool("typeof globalThis.hasOwnProperty === 'function'"));
}

#[test]
fn window_screen_and_performance_expose_browser_runtime_baseline() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "[
                typeof Window,
                window instanceof Window,
                Window.prototype instanceof EventTarget,
                Object.getPrototypeOf(window) === Window.prototype,
                Object.prototype.toString.call(window),
                window === self,
                window === frames,
                window === parent,
                window === top,
                document.defaultView === window,
                screen instanceof Screen,
                screen === window.screen,
                screen.width,
                screen.height,
                screen.availWidth,
                screen.availHeight,
                screen.colorDepth,
                screen.pixelDepth,
                performance instanceof Performance,
                performance === window.performance,
                typeof performance.timeOrigin,
                typeof performance.now(),
                performance.now() >= 0
            ].join('|')"
        ),
        "function|true|true|true|[object Window]|true|true|true|true|true|true|true|800|600|800|600|24|24|true|true|number|number|true"
    );
    assert!(h.eval_bool(
        "(() => { try { new Window(); return false; } catch (e) { return e instanceof TypeError; } })()"
    ));
    assert!(h.eval_bool(
        "(() => { try { new Screen(); return false; } catch (e) { return e instanceof TypeError; } })()"
    ));
    assert!(h.eval_bool(
        "(() => { try { new Performance(); return false; } catch (e) { return e instanceof TypeError; } })()"
    ));
    assert!(h.eval_bool(
        "(() => { const a = performance.now(); const b = performance.now(); return b >= a; })()"
    ));
    assert!(h.eval_bool("performance.timeOrigin === performance.timeOrigin"));

    // `Screen` attributes are read-only accessors on the prototype, not own
    // data properties that a page could overwrite.
    assert!(h.eval_bool(
        "['width', 'height', 'availWidth', 'availHeight', 'colorDepth', 'pixelDepth']
           .every(name => {
             const d = Object.getOwnPropertyDescriptor(Screen.prototype, name);
             return typeof d.get === 'function' && d.set === undefined
                 && !Object.prototype.hasOwnProperty.call(screen, name);
           })"
    ));

    // Brand checks: an unrelated receiver is rejected, but WebIDL's ES binding
    // substitutes the global for a null/undefined receiver on Window.
    assert!(h.eval_bool(
        "(() => {
           const rejects = fn => { try { fn(); return false; } catch (e) { return e instanceof TypeError; } };
           return rejects(() => Object.getOwnPropertyDescriptor(Screen.prototype, 'width').get.call({}))
               && rejects(() => Performance.prototype.now.call({}))
               && rejects(() => Window.prototype.matchMedia.call({}, 'all'))
               && rejects(() => Object.getOwnPropertyDescriptor(MediaQueryList.prototype, 'matches').get.call({}))
               && Window.prototype.matchMedia.call(undefined, 'all') instanceof MediaQueryList
               && Window.prototype.matchMedia.call(null, 'all') instanceof MediaQueryList;
         })()"
    ));
}

#[test]
fn match_media_uses_the_style_device_and_exposes_event_target_contract() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => {
                const m = matchMedia('(min-width: 700px) and (max-width: 900px)');
                const invalid = matchMedia('not a valid media query ???');
                return [
                    typeof matchMedia,
                    m instanceof MediaQueryList,
                    m instanceof EventTarget,
                    m.matches,
                    m.media,
                    matchMedia('screen').matches,
                    matchMedia('print').matches,
                    matchMedia('(resolution: 1dppx)').matches,
                    invalid.matches,
                    m !== matchMedia(m.media),
                    typeof m.addListener,
                    typeof m.removeListener,
                    m.onchange === null
                ].join('|');
            })()"
        ),
        "function|true|true|true|(min-width: 700px) and (max-width: 900px)|true|false|true|false|true|function|function|true"
    );
}

#[test]
fn navigator_exposes_honest_stable_practical_baseline() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "[
                navigator instanceof Navigator,
                navigator === clientInformation,
                navigator.userAgent.includes('OxidePage/'),
                !navigator.userAgent.includes('Chrome'),
                navigator.vendor,
                navigator.appCodeName,
                navigator.appName,
                navigator.product,
                navigator.productSub,
                navigator.vendorSub,
                navigator.language,
                navigator.languages.join(','),
                Object.isFrozen(navigator.languages),
                navigator.languages === navigator.languages,
                navigator.onLine,
                navigator.cookieEnabled,
                navigator.hardwareConcurrency >= 1,
                navigator.hardwareConcurrency <= 8,
                navigator.webdriver,
                navigator.maxTouchPoints,
                navigator.pdfViewerEnabled,
                navigator.javaEnabled(),
                navigator.plugins instanceof PluginArray,
                navigator.plugins === navigator.plugins,
                navigator.plugins.length,
                navigator.plugins.item(0) === null,
                navigator.plugins.namedItem('x') === null,
                navigator.mimeTypes instanceof MimeTypeArray,
                navigator.mimeTypes === navigator.mimeTypes,
                navigator.mimeTypes.length,
                Object.getOwnPropertyDescriptor(Navigator.prototype, 'userAgent').set === undefined
            ].join('|')"
        ),
        "true|true|true|true||Mozilla|Netscape|Gecko|20030107||en-US|en-US|true|true|true|true|true|true|false|0|false|false|true|true|0|true|true|true|true|0|true"
    );
    assert!(h.eval_bool(
        "(() => { try { new Navigator(); return false; } catch (e) { return e instanceof TypeError; } })()"
    ));
    assert!(h.eval_bool(
        "(() => { const before = navigator.userAgent; navigator.userAgent = 'spoof'; return navigator.userAgent === before; })()"
    ));
    assert!(h.eval_bool(
        "Object.getOwnPropertyDescriptor(globalThis, 'navigator').get instanceof Function"
    ));
}

#[test]
fn wrapper_identity_holds() {
    let h = Harness::new(PAGE);
    assert!(h.eval_bool("document.getElementById('main') === document.getElementById('main')"));
    assert!(h.eval_bool("document.body.firstElementChild === document.getElementById('main')"));
    assert!(h.eval_bool("document.body.childNodes === document.body.childNodes"));
    assert!(h.eval_bool("document.body.children === document.body.children"));
}

#[test]
fn selectors_and_collections() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_number("document.querySelectorAll('p.para').length"),
        2.0
    );
    assert_eq!(
        h.eval_string("document.querySelector('#main > p').textContent"),
        "first"
    );
    assert_eq!(
        h.eval_string("document.querySelectorAll('p')[1].textContent"),
        "second"
    );
    assert_eq!(
        h.eval_number("document.getElementsByTagName('p').length"),
        2.0
    );
    assert_eq!(
        h.eval_number("document.getElementsByClassName('para').length"),
        2.0
    );
    assert!(h.eval_bool("document.getElementById('main').matches('.box.big')"));
    assert_eq!(
        h.eval_string("document.querySelector('span').closest('#main').id"),
        "main"
    );
    // Iteration protocols on NodeList.
    assert_eq!(
        h.eval_string("[...document.querySelectorAll('p')].map(p => p.textContent).join(',')"),
        "first,second"
    );
    // Live collections reflect mutations.
    assert!(h.eval_bool(
        "(() => {
            const live = document.getElementsByTagName('p');
            const before = live.length;
            const p = document.createElement('p');
            document.getElementById('main').appendChild(p);
            return live.length === before + 1;
        })()"
    ));
    // Invalid selector → DOMException SyntaxError.
    assert!(h.eval_bool(
        "(() => { try { document.querySelector('!!'); return false; } \
         catch (e) { return e instanceof DOMException && e.name === 'SyntaxError'; } })()"
    ));
}

#[test]
fn element_attributes_is_a_live_indexed_named_node_map() {
    let h = Harness::new("<div id='target' data-mode='on'></div>");
    assert_eq!(
        h.eval_string(
            "(() => {
                const el = document.getElementById('target');
                const attrs = el.attributes;
                const first = attrs.getNamedItem('id');
                const initial = [
                    attrs === el.attributes,
                    attrs.length,
                    attrs[0].name,
                    first.localName,
                    first.value,
                    first.ownerElement === el
                ].join(':');
                el.setAttribute('title', 'hello');
                first.value = 'renamed';
                return initial + ':' + attrs.length + ':' + el.id + ':' + attrs.title.value;
            })()"
        ),
        "true:2:id:id:target:true:3:renamed:hello"
    );
}

#[test]
fn html_script_element_reflects_practical_classic_script_properties() {
    let h = Harness::new("<script id='existing'></script>");
    assert_eq!(
        h.eval_string(
            "(() => {
                const script = document.createElement('script');
                const initiallyAsync = script.async;
                script.async = false;
                const explicitlyOrdered = !script.async;
                script.src = '/assets/app.js';
                script.type = 'text/javascript';
                script.async = true;
                script.defer = true;
                script.noModule = true;
                script.crossOrigin = 'anonymous';
                script.text = 'globalThis.fromText = true';
                return [
                    script instanceof HTMLScriptElement,
                    document.getElementById('existing') instanceof HTMLScriptElement,
                    initiallyAsync,
                    explicitlyOrdered,
                    script.src,
                    script.getAttribute('src'),
                    script.type,
                    script.async,
                    script.defer,
                    script.noModule,
                    script.crossOrigin,
                    script.text,
                    script.textContent
                ].join('|');
            })()"
        ),
        "true|true|true|true|/assets/app.js|/assets/app.js|text/javascript|true|true|true|anonymous|globalThis.fromText = true|globalThis.fromText = true"
    );
}

#[test]
fn dom_mutation_from_js() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => {
                const div = document.createElement('div');
                div.id = 'fresh';
                div.innerHTML = '<b>bold</b> text';
                document.body.appendChild(div);
                return document.getElementById('fresh').innerHTML;
            })()"
        ),
        "<b>bold</b> text"
    );
    assert_eq!(
        h.eval_string(
            "(() => {
                const el = document.getElementById('main');
                el.setAttribute('data-mode', 'on');
                return el.getAttribute('data-mode');
            })()"
        ),
        "on"
    );
    assert!(h.eval_bool(
        "(() => {
            const p = document.querySelector('p');
            const parent = p.parentNode;
            parent.removeChild(p);
            return document.querySelectorAll('p').length === 1 && p.parentNode === null;
        })()"
    ));
    // HierarchyRequestError surfaces as a DOMException.
    assert!(h.eval_bool(
        "(() => { try { document.body.appendChild(document); return false; } \
         catch (e) { return e instanceof DOMException && e.name === 'HierarchyRequestError'; } })()"
    ));
}

#[test]
fn class_list_token_operations() {
    let h = Harness::new(PAGE);
    assert!(h.eval_bool(
        "(() => {
            const el = document.getElementById('main');
            if (el.classList.length !== 2) return false;
            if (!el.classList.contains('box')) return false;
            el.classList.add('extra');
            el.classList.remove('big');
            el.classList.toggle('flip');
            return el.className === 'box extra flip' && el.classList[0] === 'box';
        })()"
    ));
}

#[test]
fn dataset_reads_writes_and_deletes_data_attributes() {
    let h = Harness::new(PAGE);
    // The span carries data-x="1".
    assert!(h.eval_bool(
        "(() => {
            const el = document.querySelector('span');
            if (el.dataset.x !== '1') return false;                  // read
            el.dataset.fooBar = 'hi';                                // write -> data-foo-bar
            if (el.getAttribute('data-foo-bar') !== 'hi') return false;
            el.setAttribute('data-live-now', 'v');                   // live view of the attribute
            if (el.dataset.liveNow !== 'v') return false;
            el.dataset.n = 5;                                        // coerced to a string
            if (el.dataset.n !== '5') return false;
            delete el.dataset.x;                                     // delete -> removes the attribute
            if (el.hasAttribute('data-x')) return false;
            return el.dataset instanceof DOMStringMap && el.dataset === el.dataset;
        })()"
    ));
}

#[test]
fn dataset_enumerates_only_data_attributes_camelcased() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => {
                const el = document.querySelector('span');
                el.setAttribute('data-a-b', '1');   // -> aB
                el.setAttribute('class', 'c');       // not data-*, must not appear
                return ['x' in el.dataset, Object.keys(el.dataset).sort().join(',')].join('|');
            })()"
        ),
        "true|aB,x"
    );
}

#[test]
fn dataset_write_rejects_hyphen_before_ascii_lowercase() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => {
                try { document.querySelector('span').dataset['foo-bar'] = 'x'; return 'no throw'; }
                catch (e) { return e.name; }
            })()"
        ),
        "SyntaxError"
    );
}

#[test]
fn events_capture_target_bubble() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => {
                const order = [];
                const main = document.getElementById('main');
                const span = document.querySelector('span');
                document.body.addEventListener('ping', () => order.push('body-capture'), true);
                document.body.addEventListener('ping', () => order.push('body-bubble'));
                main.addEventListener('ping', e => order.push('main:' + e.eventPhase));
                span.addEventListener('ping', e => {
                    order.push('target:' + (e.target === span) + ':' + (e.currentTarget === span));
                });
                span.dispatchEvent(new Event('ping', { bubbles: true }));
                return order.join(',');
            })()"
        ),
        "body-capture,target:true:true,main:3,body-bubble"
    );
    // stopPropagation, once, removeEventListener.
    assert_eq!(
        h.eval_number(
            "(() => {
                let count = 0;
                const el = document.getElementById('main');
                const handler = () => { count += 1; };
                el.addEventListener('x', handler);
                el.addEventListener('x', handler); // dedup
                el.dispatchEvent(new Event('x'));
                el.removeEventListener('x', handler);
                el.dispatchEvent(new Event('x'));
                el.addEventListener('y', () => { count += 10; }, { once: true });
                el.dispatchEvent(new Event('y'));
                el.dispatchEvent(new Event('y'));
                return count;
            })()"
        ),
        11.0
    );
    // preventDefault and the dispatchEvent return value.
    assert!(h.eval_bool(
        "(() => {
            const el = document.body;
            el.addEventListener('go', e => e.preventDefault());
            const notCanceled = el.dispatchEvent(new Event('go', { cancelable: true }));
            return notCanceled === false;
        })()"
    ));
    // Window participates in propagation of connected-tree events.
    assert!(h.eval_bool(
        "(() => {
            let seen = false;
            window.addEventListener('up', () => { seen = true; });
            document.body.dispatchEvent(new Event('up', { bubbles: true }));
            return seen;
        })()"
    ));
    // CustomEvent detail.
    assert_eq!(
        h.eval_number(
            "(() => {
                let got = 0;
                document.body.addEventListener('data', e => { got = e.detail.n; });
                document.body.dispatchEvent(new CustomEvent('data', { detail: { n: 7 } }));
                return got;
            })()"
        ),
        7.0
    );
}

#[test]
fn abort_signal_removes_its_listeners() {
    let h = Harness::new(PAGE);
    // `addEventListener(..., { signal })`: aborting the signal removes the
    // listener. An *already*-aborted signal never adds it, and an explicit
    // `null` signal is a TypeError (the IDL member is non-nullable).
    assert!(h.eval_bool(
        "(() => {
            const el = document.getElementById('main');
            const ac = new AbortController();
            let count = 0;
            el.addEventListener('sig', () => { count++; }, { signal: ac.signal });
            el.dispatchEvent(new Event('sig'));
            ac.abort();
            el.dispatchEvent(new Event('sig'));
            const removedOnAbort = count === 1;

            // An already-aborted signal: the listener is never added.
            let never = 0;
            el.addEventListener('sig2', () => { never++; }, { signal: ac.signal });
            el.dispatchEvent(new Event('sig2'));

            let threw = false;
            try {
                el.addEventListener('sig3', () => {}, { signal: null });
            } catch (e) {
                threw = e instanceof TypeError;
            }
            return removedOnAbort && never === 0 && threw;
        })()"
    ));
}

#[test]
fn constructed_event_target_dispatches() {
    let h = Harness::new(PAGE);
    // `new EventTarget()` is in no tree, so it is its own whole propagation
    // path, and `event.target` must be the very object the script constructed.
    assert!(h.eval_bool(
        "(() => {
            const target = new EventTarget();
            const event = new Event('foo');
            let seen = null;
            target.addEventListener('foo', function (e) {
                seen = e === event
                    && e.target === target
                    && e.currentTarget === target
                    && e.composedPath().length === 1
                    && e.composedPath()[0] === target;
            }, { once: true });
            target.dispatchEvent(event);
            // `once` removed it; a second dispatch must not re-enter.
            let again = false;
            target.addEventListener('bar', () => { again = true; });
            target.dispatchEvent(new Event('bar'));
            return seen === true && again === true;
        })()"
    ));
}

#[test]
fn composed_path_is_empty_outside_a_dispatch() {
    let h = Harness::new(PAGE);
    // The last step of dispatch empties the event's path. `composedPath()` is
    // only meaningful *during* a dispatch; afterwards it reports `[]`, and
    // `currentTarget` is null — while `target` survives.
    assert!(h.eval_bool(
        "(() => {
            const el = document.getElementById('main');
            const event = new Event('cp');
            let during = 0;
            el.addEventListener('cp', e => { during = e.composedPath().length; });
            el.dispatchEvent(event);
            return during > 0
                && event.composedPath().length === 0
                && event.currentTarget === null
                && event.target === el;
        })()"
    ));
}

#[test]
fn event_legacy_members() {
    let h = Harness::new(PAGE);
    // srcElement is an alias for target.
    assert!(h.eval_bool(
        "(() => {
            let ok = false;
            const el = document.getElementById('main');
            el.addEventListener('se', e => { ok = e.srcElement === e.target && e.target === el; });
            el.dispatchEvent(new Event('se'));
            return ok;
        })()"
    ));
    // cancelBubble = true stops propagation; cancelBubble = false afterwards
    // does not un-stop it.
    assert_eq!(
        h.eval_string(
            "(() => {
                const log = [];
                const main = document.getElementById('main');
                const span = document.querySelector('span');
                main.addEventListener('cb', () => log.push('main'));
                span.addEventListener('cb', e => {
                    e.cancelBubble = true;
                    e.cancelBubble = false;
                    log.push('span:' + e.cancelBubble);
                });
                span.dispatchEvent(new Event('cb', { bubbles: true }));
                return log.join(',');
            })()"
        ),
        "span:true"
    );
    // returnValue is true before and false after preventDefault().
    assert!(h.eval_bool(
        "(() => {
            const e = new Event('rv', { cancelable: true });
            if (e.returnValue !== true) return false;
            e.preventDefault();
            return e.returnValue === false;
        })()"
    ));
    // returnValue = false cancels a cancelable event; returnValue = true
    // afterwards does not un-cancel it.
    assert!(h.eval_bool(
        "(() => {
            const e = new Event('rv2', { cancelable: true });
            e.returnValue = false;
            if (!e.defaultPrevented) return false;
            e.returnValue = true;
            return e.defaultPrevented === true && e.returnValue === false;
        })()"
    ));
    // A non-cancelable event stays uncancelled even if returnValue is set to false.
    assert!(h.eval_bool(
        "(() => {
            const e = new Event('rv3');
            e.returnValue = false;
            return e.defaultPrevented === false && e.returnValue === true;
        })()"
    ));
}

/// Regression: passive event listeners (DOM §2.8). `preventDefault()` and the
/// `returnValue = false` setter must be no-ops while the in-passive-listener
/// flag is set, so `defaultPrevented` stays false and `dispatchEvent` still
/// returns `true`. A passive listener must not affect any other listener on
/// the same target, and `addEventListener` must dedup on `(type, callback,
/// capture)` only — `passive` is not part of listener identity.
#[test]
fn event_listener_passive() {
    let h = Harness::new(PAGE);
    // preventDefault() is ignored inside a passive listener.
    assert!(h.eval_bool(
        "(() => {
            const et = document.createElement('div');
            let defaultPrevented;
            et.addEventListener('go', e => {
                e.preventDefault();
                defaultPrevented = e.defaultPrevented;
            }, { passive: true });
            const notCanceled = et.dispatchEvent(new Event('go', { cancelable: true }));
            return defaultPrevented === false && notCanceled === true;
        })()"
    ));
    // returnValue = false is likewise ignored inside a passive listener.
    assert!(h.eval_bool(
        "(() => {
            const et = document.createElement('div');
            let defaultPrevented;
            et.addEventListener('go', e => {
                e.returnValue = false;
                defaultPrevented = e.defaultPrevented;
            }, { passive: true });
            const notCanceled = et.dispatchEvent(new Event('go', { cancelable: true }));
            return defaultPrevented === false && notCanceled === true;
        })()"
    ));
    // A non-passive listener on the same target still cancels normally, and a
    // passive listener does not blunt it (order-independent: passive first).
    assert!(h.eval_bool(
        "(() => {
            const et = document.createElement('div');
            et.addEventListener('go', e => e.preventDefault(), { passive: true });
            et.addEventListener('go', e => e.preventDefault());
            const notCanceled = et.dispatchEvent(new Event('go', { cancelable: true }));
            return notCanceled === false;
        })()"
    ));
    // Outside any listener (or once the passive listener returns) the flag no
    // longer applies: a later dispatch's preventDefault still cancels.
    assert!(h.eval_bool(
        "(() => {
            const et = document.createElement('div');
            et.addEventListener('once-passive', e => e.preventDefault(), { passive: true });
            et.dispatchEvent(new Event('once-passive', { cancelable: true }));
            et.addEventListener('plain', e => e.preventDefault());
            const notCanceled = et.dispatchEvent(new Event('plain', { cancelable: true }));
            return notCanceled === false;
        })()"
    ));
    // addEventListener dedups on (type, callback, capture) only: re-adding
    // the same callback with a different `passive` value does not add a
    // second listener, nor does it flip the first registration's flag.
    assert_eq!(
        h.eval_number(
            "(() => {
                const et = document.createElement('div');
                let calls = 0;
                let defaultPrevented;
                const handler = e => {
                    calls += 1;
                    e.preventDefault();
                    defaultPrevented = e.defaultPrevented;
                };
                et.addEventListener('dedup', handler, { passive: true });
                et.addEventListener('dedup', handler, { passive: false });
                et.dispatchEvent(new Event('dedup', { cancelable: true }));
                // calls === 1 (deduped) and defaultPrevented === false (the
                // first registration's passive:true flag won, unmodified by
                // the second addEventListener call).
                return calls + (defaultPrevented ? 10 : 0);
            })()"
        ),
        1.0
    );
    // A console warning is emitted when preventDefault is swallowed.
    h.eval(
        "(() => {
            const et = document.createElement('div');
            et.addEventListener('warn', e => e.preventDefault(), { passive: true });
            et.dispatchEvent(new Event('warn', { cancelable: true }));
        })()",
    )
    .expect("eval");
    assert!(
        h.hooks
            .console
            .borrow()
            .iter()
            .any(|m| m.level == ConsoleLevel::Warn && m.message.contains("passive"))
    );
}

#[test]
fn mutation_observer_delivery() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => {
                globalThis.result = 'pending';
                const main = document.getElementById('main');
                const mo = new MutationObserver((records, observer) => {
                    globalThis.result = records.map(r => r.type).join(',') +
                        ':' + (observer === mo) +
                        ':' + records[0].addedNodes.length;
                });
                mo.observe(main, { childList: true, attributes: true });
                main.appendChild(document.createElement('i'));
                main.setAttribute('data-k', 'v');
                return 'armed';
            })()"
        ),
        "armed"
    );
    // The checkpoint after eval delivers the records.
    assert_eq!(
        h.eval_string("globalThis.result"),
        "childList,attributes:true:1"
    );

    // takeRecords drains without a callback.
    assert!(h.eval_bool(
        "(() => {
            const el = document.body;
            const mo = new MutationObserver(() => {});
            mo.observe(el, { attributes: true, attributeOldValue: true });
            el.setAttribute('data-t', '1');
            const records = mo.takeRecords();
            return records.length === 1 && records[0].attributeName === 'data-t';
        })()"
    ));
}

/// Regression: script `takeRecords()` must not clear transient registered
/// observers. After removing an observed subtree (which registers a transient
/// observer on it), draining records must keep observing the removed subtree so
/// later same-task mutations are still recorded.
#[test]
fn take_records_keeps_transient_registration() {
    let h = Harness::new(PAGE);
    assert!(h.eval_bool(
        "(() => {
            const mo = new MutationObserver(() => {});
            const parent = document.getElementById('main');
            const child = document.createElement('div');
            parent.appendChild(child);
            mo.observe(parent, { childList: true, subtree: true });
            parent.removeChild(child); // registers a transient observer on child
            const first = mo.takeRecords(); // drains the removal record
            child.appendChild(document.createElement('span')); // in removed subtree
            const second = mo.takeRecords(); // transient must still be active
            return first.length === 1 && first[0].type === 'childList'
                && second.length === 1 && second[0].type === 'childList';
        })()"
    ));
}

/// Regression: `createElementNS` in the HTML namespace must synchronously run a
/// defined custom element's constructor, matching `createElement`.
#[test]
fn create_element_ns_upgrades_html_custom_element() {
    let h = Harness::new("<html><body></body></html>");
    assert!(h.eval_bool(
        "(() => {
            class XCreateNs extends HTMLElement { constructor(){ super(); this.upgraded = true; } }
            customElements.define('x-create-ns', XCreateNs);
            const el = document.createElementNS('http://www.w3.org/1999/xhtml', 'x-create-ns');
            return el.upgraded === true && el instanceof XCreateNs;
        })()"
    ));
}

/// Regression: an event on a slotted node traverses the flat tree, so a listener
/// on a shadow-tree ancestor wrapping the `<slot>` fires for a composed bubbling
/// event; a non-composed event stays inside the shadow tree it is dispatched in.
#[test]
fn event_path_traverses_flat_tree_through_slots() {
    let h = Harness::new(PAGE);
    h.eval(
        "var host = document.getElementById('main'); \
         var sr = host.attachShadow({mode:'open'}); \
         sr.innerHTML = '<div id=\"wrap\"><slot></slot></div>'; \
         var wrap = sr.getElementById('wrap'); \
         var light = document.createElement('span'); \
         host.appendChild(light);", // slotted into the default slot
    )
    .unwrap();
    // (a) A bubbling composed event on the slotted child reaches the wrapper div
    // that lives inside the shadow tree around the slot.
    assert!(h.eval_bool(
        "(() => { window.log = []; \
          wrap.addEventListener('flatev', () => window.log.push('wrap')); \
          light.dispatchEvent(new CustomEvent('flatev', {bubbles:true, composed:true})); \
          return window.log.join(',') === 'wrap'; })()"
    ));
    // (b) A non-composed event dispatched inside the shadow tree stops at the
    // shadow root and never reaches the host.
    assert!(h.eval_bool(
        "(() => { window.log = []; \
          host.addEventListener('stopev', () => window.log.push('host')); \
          sr.addEventListener('stopev', () => window.log.push('sr')); \
          wrap.dispatchEvent(new CustomEvent('stopev', {bubbles:true})); \
          return window.log.join(',') === 'sr'; })()"
    ));
}

/// Regression: after an upgrade, the initial `attributeChangedCallback` must
/// fire for a namespaced attribute present at upgrade time (not only null-ns
/// attributes), with the attribute's namespace.
#[test]
fn upgrade_delivers_initial_callback_for_namespaced_attribute() {
    let h = Harness::new("<html><body></body></html>");
    h.eval(
        "window.log = []; \
         const el = document.createElement('x-ns-attr'); \
         el.setAttributeNS('urn:test', 'p:foo', 'bar'); \
         document.body.appendChild(el); \
         class XNsAttr extends HTMLElement { \
           static get observedAttributes(){ return ['foo']; } \
           attributeChangedCallback(n,o,v,ns){ window.log.push(n+'='+v+'@'+ns); } \
         } \
         customElements.define('x-ns-attr', XNsAttr);",
    )
    .unwrap();
    assert_eq!(h.eval_string("window.log.join(',')"), "foo=bar@urn:test");
}

#[test]
fn node_algorithms_via_js() {
    let h = Harness::new(PAGE);
    assert!(h.eval_bool(
        "(() => {
            const main = document.getElementById('main');
            const clone = main.cloneNode(true);
            return clone.isEqualNode(main) && !clone.isSameNode(main) && clone.id === 'main';
        })()"
    ));
    assert!(h.eval_bool(
        "(() => {
            const [a, b] = document.querySelectorAll('p');
            return (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
        })()"
    ));
    assert_eq!(h.eval_number("Node.ELEMENT_NODE"), 1.0);
    assert!(h.eval_bool("document.body.contains(document.querySelector('span'))"));
    // ChildNode/ParentNode mixins.
    assert_eq!(
        h.eval_string(
            "(() => {
                const main = document.getElementById('main');
                main.append('tail-text');
                main.prepend(document.createElement('em'));
                return main.firstElementChild.tagName + ':' + main.lastChild.textContent;
            })()"
        ),
        "EM:tail-text"
    );
}

#[test]
fn console_and_dom_exception_surface() {
    let h = Harness::new(PAGE);
    h.eval("console.log('hello', 1, true)").unwrap();
    h.eval("console.error('bad')").unwrap();
    let console = h.hooks.console.borrow();
    assert_eq!(
        (console[0].level, console[0].message.as_str()),
        (ConsoleLevel::Log, "hello 1 true")
    );
    assert_eq!(
        (console[1].level, console[1].message.as_str()),
        (ConsoleLevel::Error, "bad")
    );
    drop(console);

    assert!(h.eval_bool(
        "(() => {
            const e = new DOMException('nope', 'NotFoundError');
            return e instanceof DOMException && e instanceof Error &&
                e.name === 'NotFoundError' && e.message === 'nope' && e.code === 8;
        })()"
    ));
}

#[test]
fn listener_errors_are_reported_not_fatal() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_number(
            "(() => {
                let after = 0;
                const el = document.body;
                el.addEventListener('t', () => { throw new Error('listener boom'); });
                el.addEventListener('t', () => { after = 42; });
                el.dispatchEvent(new Event('t'));
                return after;
            })()"
        ),
        42.0
    );
    assert!(
        h.hooks
            .errors
            .borrow()
            .iter()
            .any(|e| e.message.contains("listener boom"))
    );
}

#[test]
fn detached_unwrapped_subtrees_are_freed_on_gc() {
    let h = Harness::new(PAGE);
    let before = h.state.dom.borrow().node_count();
    h.eval(
        "(() => {
            for (let i = 0; i < 50; i++) {
                const div = document.createElement('div');
                div.innerHTML = '<span>x</span>';
            }
        })()",
    )
    .unwrap();
    let grown = h.state.dom.borrow().node_count();
    assert!(grown > before, "creations must allocate nodes");
    h.realm.run_gc();
    let finalized = h.realm.take_finalized();
    oxidepage_bindings::process_finalized(&h.state, finalized);
    let after = h.state.dom.borrow().node_count();
    assert!(
        after < grown,
        "GC + finalizer processing must free detached trees ({grown} -> {after})"
    );
}

#[test]
fn connected_node_wrapper_expando_survives_gc() {
    // A node kept alive only by tree connectedness (no JS reference to its
    // wrapper) must keep its author-set expando properties across a GC: the
    // weak wrapper cache would otherwise re-mint the wrapper and silently drop
    // them, which breaks jQuery/Angular (they stash data-cache ids and
    // directive controllers on the wrapper). Regression for the angularjs.org
    // `$compile:ctreq` mis-render.
    let h = Harness::new(PAGE);
    h.eval(
        "(() => {
            const el = document.createElement('div');
            el.id = 'probe';
            document.body.appendChild(el);   // connected
            el.__expando = 'kept';
            el.nested = { data: { ctrl: 'C' } };
        })()", // the only reference to the wrapper is now the weak cache
    )
    .unwrap();
    h.realm.run_gc();
    let finalized = h.realm.take_finalized();
    oxidepage_bindings::process_finalized(&h.state, finalized);
    assert_eq!(
        h.eval_string("document.getElementById('probe').__expando"),
        "kept",
        "connected node's expando must survive GC"
    );
    assert_eq!(
        h.eval_string("document.getElementById('probe').nested.data.ctrl"),
        "C",
        "nested expando object identity must survive GC"
    );
}

#[test]
fn disconnected_wrapped_nodes_are_freed_on_gc() {
    // The connected-wrapper retention must be *released* on disconnect, or
    // detached wrapped subtrees would leak until navigation. Each element is
    // connected (retained), given an expando, then removed (retention dropped
    // at the host-call boundary); GC must then reclaim them.
    let h = Harness::new(PAGE);
    let before = h.state.dom.borrow().node_count();
    h.eval(
        "(() => {
            for (let i = 0; i < 50; i++) {
                const el = document.createElement('div');
                document.body.appendChild(el);   // connect -> retained
                el.__data = i;                   // author expando
                el.remove();                     // disconnect -> retention dropped
            }
        })()",
    )
    .unwrap();
    let grown = h.state.dom.borrow().node_count();
    assert!(grown > before, "creations must allocate nodes");
    h.realm.run_gc();
    let finalized = h.realm.take_finalized();
    oxidepage_bindings::process_finalized(&h.state, finalized);
    let after = h.state.dom.borrow().node_count();
    assert!(
        after < grown,
        "disconnected wrapped nodes must free on GC, not stay retained ({grown} -> {after})"
    );
}

#[test]
fn microtasks_queue_microtask_and_promises() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "globalThis.order = [];
             queueMicrotask(() => order.push('mt1'));
             Promise.resolve().then(() => order.push('p1'));
             queueMicrotask(() => order.push('mt2'));
             'ok'"
        ),
        "ok"
    );
    assert_eq!(h.eval_string("order.join(',')"), "mt1,p1,mt2");
}

// === Phase 3: URL, URLSearchParams, Headers, Response, document.cookie ===

#[test]
fn url_parsing_and_members() {
    let h = Harness::new("<html></html>");
    assert_eq!(
        h.eval_string("new URL('https://a.test:8080/p?x=1#h').hostname"),
        "a.test"
    );
    assert_eq!(
        h.eval_string("new URL('https://a.test:8080/p').host"),
        "a.test:8080"
    );
    assert_eq!(
        h.eval_string("new URL('https://a.test:8080/p').port"),
        "8080"
    );
    assert_eq!(
        h.eval_string("new URL('https://a.test/p?x=1#h').search"),
        "?x=1"
    );
    assert_eq!(h.eval_string("new URL('https://a.test/p#h').hash"), "#h");
    assert_eq!(
        h.eval_string("new URL('https://a.test/p').protocol"),
        "https:"
    );
    // Relative resolution against a base.
    assert_eq!(
        h.eval_string("new URL('foo', 'https://a.test/dir/').href"),
        "https://a.test/dir/foo"
    );
    // stringifier.
    assert_eq!(
        h.eval_string("String(new URL('https://a.test/'))"),
        "https://a.test/"
    );
    // Setter.
    assert_eq!(
        h.eval_string(
            "(()=>{const u=new URL('https://a.test/'); u.pathname='/x'; return u.href})()"
        ),
        "https://a.test/x"
    );
    // Statics.
    assert!(h.eval_bool("URL.canParse('https://a.test/')"));
    assert!(!h.eval_bool("URL.canParse('::::')"));
    assert_eq!(h.eval_string("URL.parse('https://a.test/').host"), "a.test");
    assert!(matches!(
        h.eval("URL.parse('nonsense')").unwrap(),
        JsValue::Null
    ));
}

#[test]
fn url_search_params() {
    let h = Harness::new("<html></html>");
    assert_eq!(
        h.eval_string("new URLSearchParams('a=1&b=2').get('a')"),
        "1"
    );
    assert_eq!(
        h.eval_string("(()=>{const p=new URLSearchParams(); p.append('x','1'); p.append('x','2'); return p.getAll('x').join(',')})()"),
        "1,2"
    );
    assert_eq!(
        h.eval_string("new URLSearchParams('a=1&a=2&b=3').toString()"),
        "a=1&a=2&b=3"
    );
    assert!(h.eval_bool("new URLSearchParams([['k','v']]).has('k')"));
    // Live view through a URL.
    assert_eq!(
        h.eval_string("(()=>{const u=new URL('https://a.test/?q=1'); u.searchParams.append('r','2'); return u.search})()"),
        "?q=1&r=2"
    );
}

#[test]
fn headers_object() {
    let h = Harness::new("<html></html>");
    assert_eq!(
        h.eval_string("(()=>{const hh=new Headers({'Content-Type':'text/html'}); return hh.get('content-type')})()"),
        "text/html"
    );
    assert!(h.eval_bool("new Headers([['a','1']]).has('A')"));
    assert!(
        !h.eval_bool("(()=>{const hh=new Headers({a:'1'}); hh.delete('a'); return hh.has('a')})()")
    );
}

#[test]
fn response_constructor_and_body() {
    let h = Harness::new("<html></html>");
    assert_eq!(
        h.eval_string("(()=>{const r=new Response('hi',{status:201,statusText:'Created'}); return r.status+':'+r.ok+':'+r.statusText})()"),
        "201:true:Created"
    );
    // text() resolves with the body (microtask checkpoint runs the reaction).
    h.eval("new Response('hello').text().then(t => { globalThis.__t = t; })")
        .unwrap();
    assert_eq!(h.eval_string("globalThis.__t"), "hello");
    // json() parses.
    h.eval("new Response('{\"n\":7}').json().then(v => { globalThis.__n = v.n; })")
        .unwrap();
    assert_eq!(h.eval_number("globalThis.__n"), 7.0);
    // arrayBuffer() resolves with the body bytes (the Uint8Array constructor
    // must run with `new`).
    h.eval(
        "new Response('AB').arrayBuffer()\
         .then(b => { globalThis.__ab = new Uint8Array(b).join(','); })",
    )
    .unwrap();
    assert_eq!(h.eval_string("globalThis.__ab"), "65,66");
}

#[test]
fn request_global_exists() {
    let h = Harness::new("<html></html>");
    assert_eq!(h.eval_string("typeof Request"), "function");
}

#[test]
fn request_constructor_defaults_and_init() {
    let h = Harness::new("<html></html>");
    // Defaults for a bare URL input.
    assert_eq!(
        h.eval_string(
            "(()=>{const r=new Request('https://example.com/a'); \
              return r.method+'|'+r.url+'|'+r.mode+'|'+r.credentials+'|'+r.redirect+'|'+r.keepalive})()"
        ),
        "GET|https://example.com/a|cors|same-origin|follow|false"
    );
    // Init overrides, method upper-cased, headers folded in.
    assert_eq!(
        h.eval_string(
            "(()=>{const r=new Request('https://example.com/a',{method:'post',\
              headers:{'Content-Type':'text/plain'},credentials:'include'}); \
              return r.method+'|'+r.credentials+'|'+r.headers.get('content-type')})()"
        ),
        "POST|include|text/plain"
    );
}

#[test]
fn request_body_consumed_once() {
    let h = Harness::new("<html></html>");
    h.eval(
        "new Request('https://example.com/',{method:'POST',body:'hi'})\
         .text().then(t => { globalThis.__rt = t; })",
    )
    .unwrap();
    assert_eq!(h.eval_string("globalThis.__rt"), "hi");
    // A GET/HEAD request with a body is a synchronous TypeError.
    assert!(
        h.eval("new Request('https://example.com/',{body:'x'})")
            .is_err()
    );
    // bodyUsed flips after consumption; a second read rejects.
    assert_eq!(
        h.eval_string(
            "(()=>{const r=new Request('https://example.com/',{method:'POST',body:'y'}); \
              r.text(); return String(r.bodyUsed)})()"
        ),
        "true"
    );
}

#[test]
fn request_from_request_copies_and_clone() {
    let h = Harness::new("<html></html>");
    // Constructing from another Request copies its state.
    assert_eq!(
        h.eval_string(
            "(()=>{const a=new Request('https://example.com/',{method:'PUT'}); \
              const b=new Request(a); return b.method+'|'+b.url})()"
        ),
        "PUT|https://example.com/"
    );
    // clone() yields an independent body; reading the clone leaves the
    // original unconsumed.
    h.eval(
        "globalThis.__a=new Request('https://example.com/',{method:'POST',body:'dup'}); \
         globalThis.__a.clone().text().then(t => { globalThis.__ct = t; })",
    )
    .unwrap();
    assert_eq!(h.eval_string("globalThis.__ct"), "dup");
    assert_eq!(h.eval_string("String(globalThis.__a.bodyUsed)"), "false");
}

#[test]
fn xhr_with_credentials_defaults_false_and_is_settable() {
    let h = Harness::new("<html></html>");
    assert_eq!(
        h.eval_string("(()=>{const x=new XMLHttpRequest(); return String(x.withCredentials)})()"),
        "false"
    );
    assert_eq!(
        h.eval_string(
            "(()=>{const x=new XMLHttpRequest(); x.withCredentials=true; return String(x.withCredentials)})()"
        ),
        "true"
    );
}

#[test]
fn document_cookie_round_trips() {
    let h = Harness::new("<html></html>");
    h.state
        .dom
        .borrow_mut()
        .set_document_url("http://example.test/".to_owned());
    assert_eq!(h.eval_string("document.cookie"), "");
    h.eval("document.cookie = 'sid=abc'").unwrap();
    h.eval("document.cookie = 'theme=dark'").unwrap();
    assert_eq!(h.eval_string("document.cookie"), "sid=abc; theme=dark");
    // HttpOnly cannot be set from script.
    h.eval("document.cookie = 'secret=1; HttpOnly'").unwrap();
    assert!(!h.eval_string("document.cookie").contains("secret"));
}

#[test]
fn url_tojson_and_params_iteration() {
    let h = Harness::new("<html></html>");
    // URL.toJSON (used by JSON.stringify).
    assert_eq!(
        h.eval_string("JSON.stringify(new URL('https://a.test/p'))"),
        "\"https://a.test/p\""
    );
    // URLSearchParams iteration.
    assert_eq!(
        h.eval_string("[...new URLSearchParams('a=1&b=2&a=3')].map(e => e.join(':')).join(',')"),
        "a:1,b:2,a:3"
    );
    assert_eq!(
        h.eval_string("[...new URLSearchParams('a=1&b=2').keys()].join(',')"),
        "a,b"
    );
    assert_eq!(
        h.eval_string("[...new URLSearchParams('a=1&b=2').values()].join(',')"),
        "1,2"
    );
    // forEach yields (value, key).
    assert_eq!(
        h.eval_string("(()=>{const o=[]; new URLSearchParams('a=1&b=2').forEach((v,k)=>o.push(k+'='+v)); return o.join(',')})()"),
        "a=1,b=2"
    );
}

// === Regression tests for the bindings/js findings ===

// [M1] A script-initiated dispatchEvent must run all listeners before the
// microtask checkpoint — microtasks a listener queues do not run between
// listeners.
#[test]
fn dispatch_event_microtasks_run_after_all_listeners() {
    let h = Harness::new(PAGE);
    // During dispatch, only the two listeners have run (no interleaved micro).
    assert_eq!(
        h.eval_string(
            "(() => {
                globalThis.log = [];
                const el = document.getElementById('main');
                el.addEventListener('x', () => {
                    Promise.resolve().then(() => log.push('micro'));
                    log.push('L1');
                });
                el.addEventListener('x', () => { log.push('L2'); });
                el.dispatchEvent(new Event('x'));
                return log.join(',');
            })()"
        ),
        "L1,L2"
    );
    // The task's microtask checkpoint (run by the harness after eval) runs the
    // queued microtask last.
    assert_eq!(h.eval_string("globalThis.log.join(',')"), "L1,L2,micro");
}

// [M2] Header names/values are validated; CR/LF cannot inject headers.
#[test]
fn header_names_and_values_are_validated() {
    let h = Harness::new("<html></html>");
    // CRLF injection in a Headers value throws TypeError.
    assert!(h.eval_bool(
        "(() => { try { new Headers().append('X', 'a\\r\\nEvil: 1'); return false; } \
          catch (e) { return e instanceof TypeError; } })()"
    ));
    // An invalid header name (not an RFC 7230 token) throws TypeError.
    assert!(h.eval_bool(
        "(() => { try { new Headers().set('bad name', '1'); return false; } \
          catch (e) { return e instanceof TypeError; } })()"
    ));
    // XHR.setRequestHeader rejects CRLF injection with a SyntaxError. The URL is
    // absolute because `open()` resolves it — the harness document is
    // `about:blank`, against which a path-relative URL has no base.
    assert!(h.eval_bool(
        "(() => { const x = new XMLHttpRequest(); x.open('GET', 'https://example.test/'); \
          try { x.setRequestHeader('X', 'a\\r\\nHost: evil'); return false; } \
          catch (e) { return e instanceof DOMException && e.name === 'SyntaxError'; } })()"
    ));
    // A valid header still works (and the value is trimmed).
    assert_eq!(
        h.eval_string(
            "(() => { const hh = new Headers(); hh.append('X-Test', ' value '); \
              return hh.get('x-test'); })()"
        ),
        "value"
    );
}

// [M3] A finalized wrapper for a still-live node must not purge that node's
// `[SameObject]` children.
#[test]
fn same_object_survives_wrapper_finalization_of_live_node() {
    let h = Harness::new(PAGE);
    // Cache the element's style, keeping only the style — the element wrapper
    // itself becomes collectable.
    h.eval("globalThis.s1 = document.getElementById('main').style; void 0;")
        .unwrap();
    h.realm.run_gc();
    let finalized = h.realm.take_finalized();
    oxidepage_bindings::process_finalized(&h.state, finalized);
    // The node is still in the document, so its cached style persists: fetching
    // the element again yields the same style object.
    assert!(h.eval_bool("document.getElementById('main').style === s1"));
    // The canonical invariant still holds.
    assert!(h.eval_bool(
        "(() => { const el = document.getElementById('main'); return el.style === el.style; })()"
    ));
}

// [M4] A lone surrogate is replaced with U+FFFD, not dropped whole.
#[test]
fn lone_surrogate_string_is_lossy_not_dropped() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => {
                const el = document.getElementById('main');
                el.setAttribute('data-x', 'abc\\uD800def');
                return el.getAttribute('data-x');
            })()"
        ),
        "abc\u{FFFD}def"
    );
    // A valid surrogate pair is untouched.
    assert_eq!(
        h.eval_string(
            "(() => {
                const el = document.getElementById('main');
                el.setAttribute('data-y', 'a\\uD83D\\uDE00b');
                return el.getAttribute('data-y');
            })()"
        ),
        "a\u{1F600}b"
    );
}

// [M5] Engine bookkeeping keeps working after page script poisons the built-ins
// it used to depend on.
#[test]
fn bookkeeping_survives_builtin_tampering() {
    let h = Harness::new(PAGE);
    assert!(h.eval_bool(
        "(() => {
            Map.prototype.set = function () { throw new Error('poisoned set'); };
            Map.prototype.get = function () { throw new Error('poisoned get'); };
            Map.prototype.has = function () { throw new Error('poisoned has'); };
            Map.prototype.delete = function () { throw new Error('poisoned delete'); };
            WeakRef.prototype.deref = function () { throw new Error('poisoned deref'); };
            Reflect.get = function () { throw new Error('poisoned reflect.get'); };
            Reflect.has = function () { throw new Error('poisoned reflect.has'); };
            Array.isArray = function () { throw new Error('poisoned isArray'); };
            Array.from = function () { throw new Error('poisoned from'); };
            // Wrapper cache identity (Map + WeakRef).
            const a = document.getElementById('main');
            const b = document.getElementById('main');
            if (a !== b) return false;
            // Collection proxy indexed + length access (Reflect).
            const ps = document.querySelectorAll('p');
            if (ps.length !== 2 || ps[0].textContent !== 'first') return false;
            // Style proxy camelCase access (Map).
            a.style.color = 'red';
            if (a.style.color !== 'red') return false;
            // Headers record init (Array.isArray / Array.from / Object.entries).
            return new Headers([['x-test', '1']]).get('x-test') === '1';
        })()"
    ));
}

// [L1] Constructors require `new`; the `.call` deviation cannot forge a usable
// instance (pins the documented behavior).
#[test]
fn constructor_requires_new_pins_behavior() {
    let h = Harness::new(PAGE);
    assert!(h.eval_bool("new Event('x') instanceof Event"));
    // Plain call (no `new`, no function `this`) is rejected.
    assert!(h.eval_bool(
        "(() => { try { Event('x'); return false; } \
          catch (e) { return e instanceof TypeError; } })()"
    ));
    // Documented deviation: `Event.call(fn, …)` slips past the `new` check but
    // yields an object with the wrong prototype — not an Event.
    assert!(h.eval_bool(
        "(() => { const o = Event.call(function fn() {}, 'x'); return !(o instanceof Event); })()"
    ));
}

// [L2] A completed XHR releases its self-referential wrapper root, so it can be
// garbage-collected.
#[test]
fn xhr_wrapper_released_after_completion() {
    let h = Harness::new("<html></html>");
    h.state
        .dom
        .borrow_mut()
        .set_document_url("http://example.test/".to_owned());
    // TestHooks hands back RequestId(1, FIRST_GENERATION) for the first send.
    h.eval("globalThis.x = new XMLHttpRequest(); x.open('GET', '/data'); x.send();")
        .unwrap();
    let id = RequestId::from_parts(1, FIRST_GENERATION);
    h.realm.with_scope(|scope| {
        let cx = BindCx {
            scope,
            state: Rc::clone(&h.state),
        };
        oxidepage_bindings::deliver_net_event(
            &cx,
            NetEvent::Headers {
                id,
                status: 200,
                status_text: "OK".to_owned(),
                headers: Vec::new(),
                final_url: "http://example.test/data".to_owned(),
                redirected: false,
                response_type: ResponseType::Basic,
            },
        );
        oxidepage_bindings::deliver_net_event(&cx, NetEvent::Done { id });
    });
    // Drop the last script reference; only the (now released) wrapper root could
    // otherwise keep the wrapper alive.
    h.eval("globalThis.x = null; void 0;").unwrap();
    h.realm.run_gc();
    let finalized = h.realm.take_finalized();
    // The XHR wrapper is a slab-backed host object (TAG_SLAB == 2).
    assert!(
        finalized.iter().any(|(tag, _)| *tag == 2),
        "XHR wrapper should be finalized after completion, got {finalized:?}"
    );
}

// [L3] Response body can be consumed only once; a second read rejects.
#[test]
fn response_body_used_rejects_second_consume() {
    let h = Harness::new("<html></html>");
    h.eval(
        "globalThis.__first = ''; globalThis.__second = '';
         const r = new Response('hi');
         r.text().then(t => { globalThis.__first = t; });
         r.text().then(
            v => { globalThis.__second = 'resolved:' + v; },
            e => { globalThis.__second = 'rejected:' + (e instanceof TypeError); }
         );",
    )
    .unwrap();
    assert_eq!(h.eval_string("globalThis.__first"), "hi");
    assert_eq!(h.eval_string("globalThis.__second"), "rejected:true");
}

// [L4] The add/remove listener dedup path (refactored to run `===` without the
// registry borrowed) still behaves correctly.
#[test]
fn add_remove_listener_dedup_capture_sensitive() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_number(
            "(() => {
                let count = 0;
                const el = document.getElementById('main');
                const handler = () => { count += 1; };
                el.addEventListener('z', handler);
                el.addEventListener('z', handler); // deduped
                el.dispatchEvent(new Event('z')); // +1
                el.removeEventListener('z', handler);
                el.dispatchEvent(new Event('z')); // no-op
                // Capture and bubble registrations are distinct.
                el.addEventListener('z', handler, true);
                el.addEventListener('z', handler, false);
                el.dispatchEvent(new Event('z')); // +2
                return count;
            })()"
        ),
        3.0
    );
}

// [L5] URL.searchParams is `[SameObject]` and a live view of the URL's query.
#[test]
fn url_search_params_is_same_object() {
    let h = Harness::new("<html></html>");
    assert!(h.eval_bool(
        "(() => { const u = new URL('https://a.test/?x=1'); \
          return u.searchParams === u.searchParams; })()"
    ));
    assert_eq!(
        h.eval_string(
            "(() => { const u = new URL('https://a.test/?x=1'); const sp = u.searchParams; \
              u.search = '?y=2'; return sp.get('y'); })()"
        ),
        "2"
    );
}

// === Per-tag HTML element interfaces ===

/// Exposes every element carrying an `id` as a global of that name, so the
/// tests below can say `a.href` instead of threading `getElementById` through
/// every assertion. (Named access on `window` is not implemented.)
const BIND_IDS: &str = "(() => {
                            const all = document.getElementsByTagName('*');
                            for (let i = 0; i < all.length; i++)
                                if (all[i].id) globalThis[all[i].id] = all[i];
                        })();";

/// A document at `http://example.test/dir/page.html` with the given body.
fn at_example(body: &str) -> Harness {
    let h = Harness::new(&format!("<!DOCTYPE html><html><body>{body}</body></html>"));
    h.state
        .dom
        .borrow_mut()
        .set_document_url("http://example.test/dir/page.html".to_owned());
    h.eval(BIND_IDS).expect("bind ids");
    h
}

#[test]
fn anchor_exposes_the_hyperlink_decomposition() {
    let h = at_example(r#"<a id="a" href="https://user:pw@host.example:8080/p/q?x=1#frag">t</a>"#);
    assert!(h.eval_bool(
        "a instanceof HTMLAnchorElement && a instanceof HTMLElement && a instanceof Element"
    ));
    assert_eq!(
        h.eval_string("a.href"),
        "https://user:pw@host.example:8080/p/q?x=1#frag"
    );
    assert_eq!(h.eval_string("a.origin"), "https://host.example:8080");
    assert_eq!(h.eval_string("a.protocol"), "https:");
    assert_eq!(h.eval_string("a.username"), "user");
    assert_eq!(h.eval_string("a.password"), "pw");
    assert_eq!(h.eval_string("a.host"), "host.example:8080");
    assert_eq!(h.eval_string("a.hostname"), "host.example");
    assert_eq!(h.eval_string("a.port"), "8080");
    assert_eq!(h.eval_string("a.pathname"), "/p/q");
    assert_eq!(h.eval_string("a.search"), "?x=1");
    assert_eq!(h.eval_string("a.hash"), "#frag");
    // The stringifier is the href getter.
    assert!(h.eval_bool("a.toString() === a.href && String(a) === a.href"));
}

#[test]
fn anchor_href_resolves_relative_urls_against_the_document() {
    let h = at_example(r#"<a id="a" href="../other.html?q=1">t</a>"#);
    assert_eq!(
        h.eval_string("a.href"),
        "http://example.test/other.html?q=1"
    );
    // The content attribute stays raw.
    assert_eq!(h.eval_string("a.getAttribute('href')"), "../other.html?q=1");
}

#[test]
fn anchor_with_no_href_decomposes_to_empty_strings() {
    let h = at_example(r#"<a id="a">t</a>"#);
    assert_eq!(h.eval_string("a.href"), "");
    assert_eq!(h.eval_string("a.protocol"), "");
    assert_eq!(h.eval_string("a.hostname"), "");
    assert_eq!(h.eval_string("a.hash"), "");
    // Setters on an unparseable href silently do nothing.
    h.eval("a.search = '?q=2'").unwrap();
    assert_eq!(h.eval_string("a.href"), "");
    assert!(h.eval_bool("a.getAttribute('href') === null"));
}

#[test]
fn anchor_component_setters_rewrite_the_href_attribute() {
    let h = at_example(r#"<a id="a" href="http://example.test/p?x=1#f">t</a>"#);
    h.eval("a.search = '?q=2'").unwrap();
    assert_eq!(h.eval_string("a.href"), "http://example.test/p?q=2#f");
    assert_eq!(
        h.eval_string("a.getAttribute('href')"),
        "http://example.test/p?q=2#f"
    );

    h.eval("a.hash = 'bottom'").unwrap();
    assert_eq!(h.eval_string("a.hash"), "#bottom");
    h.eval("a.hostname = 'other.test'").unwrap();
    assert_eq!(h.eval_string("a.host"), "other.test");
    h.eval("a.port = '8443'").unwrap();
    assert_eq!(
        h.eval_string("a.href"),
        "http://other.test:8443/p?q=2#bottom"
    );
    h.eval("a.protocol = 'https:'").unwrap();
    assert_eq!(h.eval_string("a.protocol"), "https:");
    h.eval("a.pathname = '/z'").unwrap();
    assert_eq!(h.eval_string("a.pathname"), "/z");

    // href itself takes the raw string and resolves on read.
    h.eval("a.href = '/root'").unwrap();
    assert_eq!(h.eval_string("a.getAttribute('href')"), "/root");
    assert_eq!(h.eval_string("a.href"), "http://example.test/root");
}

#[test]
fn anchor_reflects_its_plain_attributes() {
    let h = at_example(
        r#"<a id="a" href="/x" rel="noopener noreferrer" target="_blank" hreflang="en" type="text/html" download="f.txt" referrerpolicy="no-referrer">hello <b>world</b></a>"#,
    );
    assert_eq!(h.eval_string("a.rel"), "noopener noreferrer");
    assert_eq!(h.eval_string("a.target"), "_blank");
    assert_eq!(h.eval_string("a.hreflang"), "en");
    assert_eq!(h.eval_string("a.type"), "text/html");
    assert_eq!(h.eval_string("a.download"), "f.txt");
    assert_eq!(h.eval_string("a.referrerPolicy"), "no-referrer");
    assert_eq!(h.eval_string("a.text"), "hello world");

    h.eval("a.text = 'replaced'").unwrap();
    assert_eq!(h.eval_string("a.textContent"), "replaced");
    assert!(h.eval_bool("a.children.length === 0"));

    assert!(h.eval_bool("a.relList instanceof DOMTokenList"));
    assert!(h.eval_bool("a.relList === a.relList"));
    assert!(h.eval_bool("a.relList.contains('noopener')"));
    h.eval("a.relList.add('nofollow')").unwrap();
    assert_eq!(
        h.eval_string("a.getAttribute('rel')"),
        "noopener noreferrer nofollow"
    );
}

/// `supports()` answers only for attributes that define supported tokens.
/// `rel` does, so it must return a boolean — Vite's modulepreload polyfill
/// gates on `link.relList.supports('modulepreload')`, and a throw there takes
/// the whole module down with it.
#[test]
fn rel_list_supports_answers_and_class_list_throws() {
    let h = at_example("<link id='l' rel='stylesheet' href='s.css'><a id='a' rel='noopener'>x</a>");
    h.eval("const link = document.getElementById('l'), anchor = document.getElementById('a');")
        .unwrap();

    // The engine fetches and applies `<link rel=stylesheet>`, so it reports it.
    assert!(h.eval_bool("link.relList.supports('stylesheet')"));
    assert!(
        h.eval_bool("link.relList.supports('STYLEsheet')"),
        "ASCII case-insensitive"
    );
    // Not implemented, so honestly unsupported — but an answer, not a throw.
    assert!(!h.eval_bool("link.relList.supports('modulepreload')"));
    assert!(!h.eval_bool("link.relList.supports('')"));
    // `rel` on <a> defines supported tokens too (an empty set): no throw.
    assert!(!h.eval_bool("anchor.relList.supports('noopener')"));

    // `class` defines no supported tokens: spec says TypeError.
    assert!(h.eval_bool(
        "(() => { try { anchor.classList.supports('x'); return false; }
                  catch (e) { return e instanceof TypeError; } })()"
    ));
}

/// `document.referrer` is a string, always — script does arithmetic on it
/// (`document.referrer.indexOf(...)`), so `undefined` is a page-breaking gap.
#[test]
fn document_referrer_is_the_empty_string() {
    let h = at_example("");
    assert_eq!(h.eval_string("typeof document.referrer"), "string");
    assert_eq!(h.eval_string("document.referrer"), "");
}

#[test]
fn created_anchors_get_the_anchor_prototype() {
    let h = at_example("");
    assert!(h.eval_bool("document.createElement('a') instanceof HTMLAnchorElement"));
    // A tag without a per-tag interface still lands on HTMLElement.
    assert!(h.eval_bool(
        "const d = document.createElement('div');
         d instanceof HTMLElement && !(d instanceof HTMLAnchorElement)"
    ));
    // The brand check rejects a foreign receiver.
    assert!(h.eval_bool(
        "try {
            Object.getOwnPropertyDescriptor(HTMLAnchorElement.prototype, 'href')
                .get.call(document.createElement('div'));
            false;
         } catch (e) { e instanceof TypeError }"
    ));
}

#[test]
fn area_exposes_the_hyperlink_decomposition_and_its_attributes() {
    let h = at_example(
        r#"<map><area id="ar" href="/p?x=1#f" alt="A" coords="0,0,10,10" shape="rect" target="_top" rel="nofollow" download="d" referrerpolicy="origin"></map>"#,
    );
    assert!(h.eval_bool("ar instanceof HTMLAreaElement"));
    assert_eq!(h.eval_string("ar.href"), "http://example.test/p?x=1#f");
    assert_eq!(h.eval_string("ar.hostname"), "example.test");
    assert_eq!(h.eval_string("ar.search"), "?x=1");
    assert_eq!(h.eval_string("ar.hash"), "#f");
    assert_eq!(h.eval_string("ar.alt"), "A");
    assert_eq!(h.eval_string("ar.coords"), "0,0,10,10");
    assert_eq!(h.eval_string("ar.shape"), "rect");
    assert_eq!(h.eval_string("ar.target"), "_top");
    assert_eq!(h.eval_string("ar.download"), "d");
    assert_eq!(h.eval_string("ar.referrerPolicy"), "origin");
    assert!(h.eval_bool("ar.relList.contains('nofollow')"));
}

#[test]
fn link_reflects_href_rel_list_as_and_disabled() {
    let h = at_example(
        r#"<link id="l" rel="preload stylesheet" href="/s.css" as="style" media="screen" type="text/css" hreflang="en" crossorigin="anonymous">"#,
    );
    assert!(h.eval_bool("l instanceof HTMLLinkElement"));
    assert_eq!(h.eval_string("l.href"), "http://example.test/s.css");
    assert_eq!(h.eval_string("l.as"), "style");
    assert_eq!(h.eval_string("l.media"), "screen");
    assert_eq!(h.eval_string("l.type"), "text/css");
    assert_eq!(h.eval_string("l.hreflang"), "en");
    assert_eq!(h.eval_string("l.crossOrigin"), "anonymous");
    assert!(h.eval_bool("l.relList.contains('stylesheet') && l.relList.contains('preload')"));
    assert!(h.eval_bool("l.disabled === false"));

    h.eval("l.disabled = true").unwrap();
    assert!(h.eval_bool("l.disabled && l.hasAttribute('disabled')"));
    h.eval("l.disabled = false").unwrap();
    assert!(h.eval_bool("!l.disabled && !l.hasAttribute('disabled')"));

    h.eval("l.as = 'font'").unwrap();
    assert_eq!(h.eval_string("l.getAttribute('as')"), "font");
    h.eval("l.crossOrigin = null").unwrap();
    assert!(h.eval_bool("l.crossOrigin === null && !l.hasAttribute('crossorigin')"));
}

#[test]
fn form_reflects_action_and_the_hyphenated_accept_charset() {
    let h = at_example(
        r#"<form id="f" action="submit.php" method="post" enctype="multipart/form-data" target="_self" name="login" accept-charset="utf-8" novalidate></form>"#,
    );
    assert!(h.eval_bool("f instanceof HTMLFormElement"));
    assert_eq!(
        h.eval_string("f.action"),
        "http://example.test/dir/submit.php"
    );
    assert_eq!(h.eval_string("f.method"), "post");
    assert_eq!(h.eval_string("f.enctype"), "multipart/form-data");
    assert_eq!(h.eval_string("f.target"), "_self");
    assert_eq!(h.eval_string("f.name"), "login");
    assert_eq!(h.eval_string("f.acceptCharset"), "utf-8");
    assert!(h.eval_bool("f.noValidate"));

    h.eval("f.acceptCharset = 'iso-8859-1'").unwrap();
    assert_eq!(
        h.eval_string("f.getAttribute('accept-charset')"),
        "iso-8859-1"
    );
    h.eval("f.noValidate = false").unwrap();
    assert!(h.eval_bool("!f.hasAttribute('novalidate')"));
}

#[test]
fn image_reports_src_and_an_undecoded_loading_state() {
    let h = at_example(
        r#"<img id="i" src="pic.png" alt="P" width="32" height="16" loading="lazy" decoding="async">"#,
    );
    assert!(h.eval_bool("i instanceof HTMLImageElement"));
    assert_eq!(h.eval_string("i.src"), "http://example.test/dir/pic.png");
    assert_eq!(h.eval_string("i.alt"), "P");
    assert_eq!(h.eval_string("i.loading"), "lazy");
    assert_eq!(h.eval_string("i.decoding"), "async");
    // Nothing has decoded: no intrinsic size, not complete, no current source.
    assert_eq!(h.eval_number("i.naturalWidth"), 0.0);
    assert_eq!(h.eval_number("i.naturalHeight"), 0.0);
    assert!(h.eval_bool("i.complete === false"));
    assert_eq!(h.eval_string("i.currentSrc"), "");
}

#[test]
fn a_detached_image_falls_back_to_its_width_attribute() {
    let h = at_example("");
    assert_eq!(
        h.eval_number(
            "(() => { const i = document.createElement('img');
                      i.setAttribute('width', '32'); return i.width; })()"
        ),
        32.0
    );
    // An image with no src has nothing left to load.
    assert!(h.eval_bool("document.createElement('img').complete"));
    assert_eq!(
        h.eval_number(
            "(() => { const i = document.createElement('img'); i.height = 48; return i.height; })()"
        ),
        48.0
    );
}

#[test]
fn every_anchor_href_is_a_string_regression() {
    // Cloudflare's email-decode.min.js walks every anchor and calls
    // `o.href.indexOf(...)` inside a try/catch. Before per-tag interfaces this
    // threw 166 TypeErrors on a real page and the decoder never ran.
    let anchors: String = (0..8)
        .map(|i| format!(r#"<a href="/cdn-cgi/l/email-protection#abc{i}">x</a>"#))
        .collect();
    let h = at_example(&anchors);
    assert!(h.eval_bool(
        "[...document.querySelectorAll('a')].every(
            o => typeof o.href === 'string' && o.href.indexOf('#') !== -1)"
    ));
    assert_eq!(h.eval_number("document.querySelectorAll('a').length"), 8.0);
}

#[test]
fn base_href_moves_reflection_and_base_uri_but_not_document_url() {
    let h = Harness::new(
        r#"<!DOCTYPE html><html><head><base href="https://cdn.example/x/"></head>
           <body><a id="a" href="y">t</a><img id="i" src="z.png"></body></html>"#,
    );
    h.state
        .dom
        .borrow_mut()
        .set_document_url("http://example.test/".to_owned());
    h.eval(BIND_IDS).expect("bind ids");
    assert_eq!(h.eval_string("a.href"), "https://cdn.example/x/y");
    assert_eq!(h.eval_string("i.src"), "https://cdn.example/x/z.png");
    assert_eq!(h.eval_string("document.baseURI"), "https://cdn.example/x/");
    // `document.URL` / `documentURI` / `location` stay the document URL.
    assert_eq!(h.eval_string("document.URL"), "http://example.test/");
    assert_eq!(
        h.eval_string("document.documentURI"),
        "http://example.test/"
    );
    assert_eq!(h.eval_string("location.href"), "http://example.test/");
}

// === Custom elements (autonomous) ===

#[test]
fn custom_elements_registry_exists() {
    let h = Harness::new("<html></html>");
    assert_eq!(h.eval_string("typeof customElements"), "object");
    assert_eq!(h.eval_string("typeof customElements.define"), "function");
    assert!(h.eval_bool("customElements instanceof CustomElementRegistry"));
    assert_eq!(h.eval_string("typeof HTMLElement"), "function");
}

#[test]
fn define_get_get_name_when_defined() {
    let h = Harness::new("<html></html>");
    assert!(h.eval_bool(
        "class XA extends HTMLElement {}
         customElements.define('x-a', XA);
         customElements.get('x-a') === XA
             && customElements.getName(XA) === 'x-a'
             && customElements.get('x-missing') === undefined
             && customElements.getName(class {}) === null"
    ));
}

/// Like [`threw`], but reports the exception's `name` (a `DOMException`'s
/// `constructor.name` is always `"DOMException"`; the specific error is `.name`).
fn threw_name(h: &Harness, source: &str) -> String {
    h.eval_string(&format!(
        "(() => {{ try {{ {source}; return 'NO THROW'; }} catch (e) {{ return e.name; }} }})()"
    ))
}

#[test]
fn define_rejects_bad_name_and_duplicates() {
    let h = Harness::new("<html></html>");
    assert_eq!(
        threw_name(
            &h,
            "customElements.define('nohyphen', class extends HTMLElement {})"
        ),
        "SyntaxError"
    );
    assert_eq!(
        threw_name(&h, "customElements.define('x-b', 123)"),
        "TypeError"
    );
    assert_eq!(
        threw_name(
            &h,
            "class XC extends HTMLElement {}
             customElements.define('x-c', XC);
             customElements.define('x-c', class extends HTMLElement {})"
        ),
        "NotSupportedError"
    );
    assert_eq!(
        threw_name(
            &h,
            "class XD extends HTMLElement {}
             customElements.define('x-d1', XD);
             customElements.define('x-d2', XD)"
        ),
        "NotSupportedError"
    );
}

#[test]
fn new_custom_element_is_disconnected_with_custom_prototype() {
    let h = Harness::new("<html></html>");
    assert!(h.eval_bool(
        "class XE extends HTMLElement { constructor(){ super(); this.marked = true; } }
         customElements.define('x-e', XE);
         const el = new XE();
         el instanceof XE
             && el instanceof HTMLElement
             && el.tagName === 'X-E'
             && el.isConnected === false
             && el.marked === true
             && Object.getPrototypeOf(el) === XE.prototype"
    ));
}

#[test]
fn illegal_constructor_for_unregistered_and_base() {
    let h = Harness::new("<html></html>");
    assert_eq!(threw(&h, "new HTMLElement()"), "TypeError");
    assert_eq!(
        threw(&h, "new (class extends HTMLElement {})()"),
        "TypeError"
    );
}

#[test]
fn create_element_upgrades_synchronously() {
    let h = Harness::new("<html></html>");
    assert!(h.eval_bool(
        "class XF extends HTMLElement { constructor(){ super(); this.up = 1; } }
         customElements.define('x-f', XF);
         const el = document.createElement('x-f');
         el instanceof XF && el.up === 1"
    ));
}

#[test]
fn when_defined_resolves_on_later_define() {
    let h = Harness::new("<html></html>");
    h.eval(
        "customElements.whenDefined('x-g').then(c => { globalThis.__wd = c.name; });
         class XG extends HTMLElement {}
         customElements.define('x-g', XG);",
    )
    .unwrap();
    assert_eq!(h.eval_string("globalThis.__wd"), "XG");
}

/// Reactions are delivered at the microtask checkpoint (task boundary), not
/// synchronously inside `appendChild`. The harness runs a checkpoint after each
/// `eval`, so the callback has fired by the time the next statement reads it.
#[test]
fn upgrade_and_connected_callback() {
    let h = Harness::new("<html><body></body></html>");
    h.eval(
        "class XH extends HTMLElement {
            connectedCallback(){ document.title = 'connected'; }
         }
         customElements.define('x-h', XH);
         document.body.appendChild(document.createElement('x-h'));",
    )
    .unwrap();
    assert_eq!(h.eval_string("document.title"), "connected");
}

// === NodeFilter + per-tag interface globals ===

#[test]
fn node_filter_constants_exist() {
    let h = Harness::new("<html></html>");
    assert_eq!(h.eval_string("typeof NodeFilter"), "function");
    assert!(h.eval_bool(
        "NodeFilter.SHOW_ELEMENT === 0x1 \
         && NodeFilter.SHOW_TEXT === 0x4 \
         && NodeFilter.SHOW_ALL === 0xFFFFFFFF \
         && NodeFilter.FILTER_ACCEPT === 1 \
         && NodeFilter.FILTER_REJECT === 2 \
         && NodeFilter.FILTER_SKIP === 3"
    ));
}

#[test]
fn per_tag_interface_globals_exist_and_instanceof_holds() {
    let h = Harness::new("<html><body></body></html>");
    assert_eq!(h.eval_string("typeof HTMLInputElement"), "function");
    assert_eq!(h.eval_string("typeof HTMLDivElement"), "function");
    assert!(h.eval_bool(
        "document.createElement('input') instanceof HTMLInputElement \
         && document.createElement('input') instanceof HTMLElement \
         && document.createElement('div') instanceof HTMLDivElement \
         && document.createElement('video') instanceof HTMLVideoElement \
         && document.createElement('video') instanceof HTMLMediaElement"
    ));
    // Subclassing the interface is allowed (definition-time); it only throws if
    // constructed directly (customized built-ins unsupported).
    assert!(h.eval_bool("(class extends HTMLInputElement {}), true"));
    assert_eq!(threw(&h, "new HTMLInputElement()"), "TypeError");
}

// === Shadow DOM (Phase 2 bindings) ===

#[test]
fn attach_shadow_returns_shadow_root() {
    let h = Harness::new(PAGE);
    h.eval(
        "var host = document.getElementById('main'); var sr = host.attachShadow({mode:'open'});",
    )
    .unwrap();
    assert!(h.eval_bool("sr instanceof ShadowRoot"));
    assert!(h.eval_bool("sr instanceof DocumentFragment && sr instanceof Node"));
    assert_eq!(h.eval_string("sr.mode"), "open");
    assert!(h.eval_bool("sr.host === host"));
    assert!(h.eval_bool("host.shadowRoot === sr"));
    assert_eq!(h.eval_number("sr.nodeType"), 11.0);
}

#[test]
fn attach_shadow_closed_mode_hides_root() {
    let h = Harness::new(PAGE);
    h.eval(
        "var host = document.getElementById('main'); var sr = host.attachShadow({mode:'closed'});",
    )
    .unwrap();
    assert_eq!(h.eval_string("sr.mode"), "closed");
    assert!(h.eval_bool("host.shadowRoot === null"));
}

#[test]
fn attach_shadow_validation_errors() {
    let h = Harness::new(PAGE);
    // Missing/invalid mode is a TypeError.
    assert!(h.eval_bool(
        "(() => { try { document.getElementById('main').attachShadow({}); return false; } \
         catch (e) { return e instanceof TypeError; } })()"
    ));
    // Repeat attach is an InvalidStateError DOMException.
    assert_eq!(
        h.eval_string(
            "(() => { const el = document.createElement('div'); el.attachShadow({mode:'open'}); \
             try { el.attachShadow({mode:'open'}); return 'no-throw'; } catch (e) { return e.name; } })()"
        ),
        "InvalidStateError"
    );
    // An unsupported host name is a NotSupportedError.
    assert_eq!(
        h.eval_string(
            "(() => { const el = document.createElement('a'); \
             try { el.attachShadow({mode:'open'}); return 'no-throw'; } catch (e) { return e.name; } })()"
        ),
        "NotSupportedError"
    );
}

#[test]
fn shadow_root_inner_html_builds_shadow_tree() {
    let h = Harness::new(PAGE);
    h.eval(
        "var host = document.getElementById('main'); var sr = host.attachShadow({mode:'open'}); \
         sr.innerHTML = '<section><slot name=\"a\"></slot><slot></slot></section>';",
    )
    .unwrap();
    assert_eq!(h.eval_number("sr.childNodes.length"), 1.0);
    assert_eq!(h.eval_string("sr.firstChild.tagName"), "SECTION");
    assert!(h.eval_bool("sr.querySelector('slot[name=a]') !== null"));
    assert!(h.eval_bool("sr.firstChild.isConnected"));
    // Serialization of the host stays light-DOM (spec).
    assert!(h.eval_bool("!host.innerHTML.includes('section')"));
    assert!(h.eval_bool("sr.innerHTML.includes('<slot name=\"a\">')"));
}

#[test]
fn get_root_node_is_shadow_aware() {
    let h = Harness::new(PAGE);
    h.eval(
        "var host = document.getElementById('main'); var sr = host.attachShadow({mode:'open'}); \
         sr.innerHTML = '<p id=\"inner\">x</p>'; var inner = sr.querySelector('p');",
    )
    .unwrap();
    assert!(h.eval_bool("inner.getRootNode() === sr"));
    assert!(h.eval_bool("inner.getRootNode({composed:false}) === sr"));
    assert!(h.eval_bool("inner.getRootNode({composed:true}) === document"));
    assert!(h.eval_bool("host.getRootNode() === document"));
}

#[test]
fn slot_assignment_and_assigned_slot() {
    let h = Harness::new(
        r#"<!DOCTYPE html><html><body>
        <div id="host"><span slot="a" id="named">n</span><b id="plain">d</b></div>
        </body></html>"#,
    );
    h.eval(
        "var host = document.getElementById('host'); var sr = host.attachShadow({mode:'open'}); \
         sr.innerHTML = '<slot name=\"a\"></slot><slot></slot>';",
    )
    .unwrap();
    assert!(h.eval_bool(
        "document.getElementById('named').assignedSlot === sr.querySelector('slot[name=a]')"
    ));
    assert!(h.eval_bool(
        "document.getElementById('plain').assignedSlot === sr.querySelectorAll('slot')[1]"
    ));
    assert_eq!(
        h.eval_number("sr.querySelector('slot[name=a]').assignedNodes().length"),
        1.0
    );
    assert!(h.eval_bool(
        "sr.querySelector('slot[name=a]').assignedElements()[0] === document.getElementById('named')"
    ));
    assert_eq!(h.eval_string("sr.querySelector('slot[name=a]').name"), "a");
    assert!(h.eval_bool("sr.querySelector('slot') instanceof HTMLSlotElement"));
}

#[test]
fn shadow_ids_are_scoped() {
    let h = Harness::new(PAGE);
    h.eval(
        "var host = document.getElementById('main'); var sr = host.attachShadow({mode:'open'}); \
         sr.innerHTML = '<p id=\"scoped\">x</p>';",
    )
    .unwrap();
    assert!(h.eval_bool("document.getElementById('scoped') === null"));
    assert!(h.eval_bool("sr.getElementById('scoped') !== null"));
}

#[test]
fn element_part_reflects_attribute() {
    let h = Harness::new(PAGE);
    h.eval("var el = document.createElement('div'); el.setAttribute('part', 'x y');")
        .unwrap();
    assert!(h.eval_bool("el.part instanceof DOMTokenList"));
    assert_eq!(h.eval_number("el.part.length"), 2.0);
    assert!(h.eval_bool("el.part.contains('x')"));
}

#[test]
fn constructable_stylesheet_and_adopted_sheets() {
    let h = Harness::new(PAGE);
    h.eval("var s = new CSSStyleSheet(); s.replaceSync('div { color: red; }');")
        .unwrap();
    assert!(h.eval_bool("s instanceof CSSStyleSheet"));
    h.eval(
        "var host = document.getElementById('main'); var sr = host.attachShadow({mode:'open'});",
    )
    .unwrap();
    // Default is an (empty) array — injectors feature-detect this.
    assert!(
        h.eval_bool("Array.isArray(sr.adoptedStyleSheets) && sr.adoptedStyleSheets.length === 0")
    );
    assert!(h.eval_bool("Array.isArray(document.adoptedStyleSheets)"));
    h.eval("sr.adoptedStyleSheets = [s];").unwrap();
    assert_eq!(h.eval_number("sr.adoptedStyleSheets.length"), 1.0);
    // The getter returns the same observable array between reads.
    assert!(h.eval_bool("sr.adoptedStyleSheets === sr.adoptedStyleSheets"));
    // ObservableArray semantics: in-place mutations work and re-sync.
    h.eval("var s2 = new CSSStyleSheet(); sr.adoptedStyleSheets.push(s2);")
        .unwrap();
    assert_eq!(h.eval_number("sr.adoptedStyleSheets.length"), 2.0);
    h.eval("sr.adoptedStyleSheets[0] = s2;").unwrap();
    assert!(h.eval_bool("sr.adoptedStyleSheets[0] === s2"));
    h.eval("sr.adoptedStyleSheets.length = 0;").unwrap();
    assert_eq!(h.eval_number("sr.adoptedStyleSheets.length"), 0.0);
    // A non-sheet entry is rejected by the sync.
    assert!(h.eval_bool(
        "(() => { try { sr.adoptedStyleSheets.push({}); return false; } \
         catch (e) { return e instanceof TypeError; } })()"
    ));
}

#[test]
fn composed_event_crosses_shadow_boundary() {
    let h = Harness::new(PAGE);
    h.eval(
        "var host = document.getElementById('main'); var sr = host.attachShadow({mode:'open'}); \
         sr.innerHTML = '<button id=\"btn\">b</button>'; \
         window.log = []; \
         document.addEventListener('x-ping', () => window.log.push('doc')); \
         host.addEventListener('x-ping', () => window.log.push('host')); \
         sr.addEventListener('x-ping', () => window.log.push('sr')); \
         var btn = sr.getElementById('btn'); \
         btn.dispatchEvent(new CustomEvent('x-ping', {bubbles:true, composed:true}));",
    )
    .unwrap();
    assert_eq!(h.eval_string("window.log.join(',')"), "sr,host,doc");
    // A non-composed event stops at the shadow root.
    h.eval(
        "window.log = []; \
         btn.dispatchEvent(new CustomEvent('x-ping', {bubbles:true}));",
    )
    .unwrap();
    assert_eq!(h.eval_string("window.log.join(',')"), "sr");
    // composedPath includes shadow nodes and crosses to the document.
    assert!(h.eval_bool(
        "(() => { let p; btn.addEventListener('x-path', e => p = e.composedPath(), {once:true}); \
         btn.dispatchEvent(new CustomEvent('x-path', {composed:true})); \
         return p[0] === btn && p.includes(sr) && p.includes(host) && p.includes(document); })()"
    ));
}

#[test]
fn indexed_getter_interfaces_are_spreadable() {
    // WebIDL: indexed-getter interfaces without `iterable<>` still get
    // @@iterator = %Array.prototype.values% (Swiper does `[...el.attributes]`).
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "[...document.getElementById('main').attributes].map(a => a.name).sort().join(',')"
        ),
        "class,id"
    );
    assert_eq!(
        h.eval_number("[...document.getElementsByTagName('p')].length"),
        2.0
    );
    // But no forEach (spec: only iterable<> declares one).
    assert!(
        h.eval_bool("typeof document.getElementById('main').attributes.forEach === 'undefined'")
    );
}

// === AbortController / AbortSignal ===

#[test]
fn abort_controller_basics() {
    let h = Harness::new(PAGE);
    // Brand and inheritance.
    assert!(h.eval_bool("new AbortController().signal instanceof AbortSignal"));
    assert!(h.eval_bool("new AbortController().signal instanceof EventTarget"));
    assert!(
        h.eval_bool("(() => { const c = new AbortController(); return c.signal === c.signal; })()")
    );
    // Illegal constructor for AbortSignal.
    assert_eq!(threw(&h, "new AbortSignal()"), "TypeError");
    // Fresh signal is not aborted; reason is undefined.
    assert!(h.eval_bool("new AbortController().signal.aborted === false"));
    assert!(h.eval_bool("new AbortController().signal.reason === undefined"));
}

#[test]
fn abort_sets_aborted_and_default_reason() {
    let h = Harness::new(PAGE);
    // Default reason is an AbortError DOMException.
    assert!(h.eval_bool(
        "(() => { const c = new AbortController(); c.abort(); \
          return c.signal.aborted === true \
            && c.signal.reason instanceof DOMException \
            && c.signal.reason.name === 'AbortError'; })()"
    ));
    // An explicit reason is preserved verbatim.
    assert!(h.eval_bool(
        "(() => { const c = new AbortController(); const r = {custom: 1}; c.abort(r); \
          return c.signal.reason === r; })()"
    ));
    // throwIfAborted throws the reason.
    assert_eq!(
        h.eval_string(
            "(() => { const c = new AbortController(); c.abort('boom'); \
              try { c.signal.throwIfAborted(); return 'NO THROW'; } catch (e) { return e; } })()"
        ),
        "boom"
    );
    // Not-aborted throwIfAborted is a no-op.
    assert!(
        h.eval_bool("(() => { new AbortController().signal.throwIfAborted(); return true; })()")
    );
}

#[test]
fn abort_fires_onabort_before_catch_reactions() {
    let h = Harness::new(PAGE);
    // onabort runs synchronously inside abort(); a queued promise reaction
    // (standing in for a fetch `.catch`) runs later at the microtask
    // checkpoint, after abort() returns. The log order proves it.
    assert_eq!(
        h.eval_string(
            "(() => { \
              window.log = []; \
              const c = new AbortController(); \
              c.signal.onabort = () => window.log.push('onabort'); \
              Promise.resolve().then(() => window.log.push('microtask')); \
              c.abort(); \
              window.log.push('after-abort'); \
              return window.log.join(','); })()"
        ),
        // onabort fires synchronously during abort(); the queued microtask has
        // not run yet when abort() returns.
        "onabort,after-abort"
    );
    // The queued microtask ran at the checkpoint after the eval.
    assert!(h.eval_bool("window.log.includes('microtask')"));
}

#[test]
fn double_abort_is_a_no_op() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_number(
            "(() => { let n = 0; const c = new AbortController(); \
              c.signal.addEventListener('abort', () => n++); \
              c.abort('first'); c.abort('second'); return n; })()"
        ),
        1.0
    );
    // The first reason is kept.
    assert_eq!(
        h.eval_string(
            "(() => { const c = new AbortController(); c.abort('first'); c.abort('second'); \
              return c.signal.reason; })()"
        ),
        "first"
    );
}

#[test]
fn abort_signal_static_factories() {
    let h = Harness::new(PAGE);
    // AbortSignal.abort() returns an already-aborted signal with a default reason.
    assert!(h.eval_bool(
        "(() => { const s = AbortSignal.abort(); \
          return s instanceof AbortSignal && s.aborted && s.reason.name === 'AbortError'; })()"
    ));
    // AbortSignal.abort(reason) preserves the reason.
    assert!(h.eval_bool("AbortSignal.abort('x').reason === 'x'"));
    // AbortSignal.timeout() returns a not-yet-aborted signal.
    assert!(h.eval_bool(
        "(() => { const s = AbortSignal.timeout(1000); \
          return s instanceof AbortSignal && s.aborted === false; })()"
    ));
}

// === structuredClone ===

#[test]
fn structured_clone_primitives_and_plain_objects() {
    let h = Harness::new(PAGE);
    assert!(h.eval_bool("structuredClone(42) === 42"));
    assert!(h.eval_bool("structuredClone('hi') === 'hi'"));
    assert!(h.eval_bool("structuredClone(true) === true"));
    assert!(h.eval_bool("structuredClone(null) === null"));
    assert!(h.eval_bool("structuredClone(undefined) === undefined"));
    assert!(h.eval_bool("structuredClone(10n) === 10n"));
    // Deep clone: nested object copied, not shared.
    assert!(h.eval_bool(
        "(() => { const o = {a: {b: 1}}; const c = structuredClone(o); \
          return c.a.b === 1 && c !== o && c.a !== o.a; })()"
    ));
    // Arrays, including holes.
    assert!(h.eval_bool(
        "(() => { const a = [1, , 3]; const c = structuredClone(a); \
          return c.length === 3 && c[0] === 1 && !(1 in c) && c[2] === 3; })()"
    ));
    // null-prototype object.
    assert!(h.eval_bool(
        "(() => { const o = Object.create(null); o.x = 5; const c = structuredClone(o); \
          return Object.getPrototypeOf(c) === null && c.x === 5; })()"
    ));
}

#[test]
fn structured_clone_preserves_cycles_and_identity() {
    let h = Harness::new(PAGE);
    // A cyclic object clones without infinite recursion, and the cycle is
    // reconstructed (the clone points back at itself).
    assert!(h.eval_bool(
        "(() => { const o = {}; o.self = o; const c = structuredClone(o); \
          return c.self === c; })()"
    ));
    // Shared identity: the same object referenced twice clones once.
    assert!(h.eval_bool(
        "(() => { const shared = {v: 1}; const o = {a: shared, b: shared}; \
          const c = structuredClone(o); return c.a === c.b && c.a !== shared; })()"
    ));
}

#[test]
fn structured_clone_builtins() {
    let h = Harness::new(PAGE);
    // Map / Set.
    assert!(h.eval_bool(
        "(() => { const m = new Map([['k', 1]]); const c = structuredClone(m); \
          return c instanceof Map && c.get('k') === 1 && c !== m; })()"
    ));
    assert!(h.eval_bool(
        "(() => { const s = new Set([1, 2]); const c = structuredClone(s); \
          return c instanceof Set && c.has(1) && c.has(2) && c !== s; })()"
    ));
    // Date / RegExp.
    assert!(h.eval_bool(
        "(() => { const d = new Date(1000); const c = structuredClone(d); \
          return c instanceof Date && c.getTime() === 1000 && c !== d; })()"
    ));
    assert!(h.eval_bool(
        "(() => { const r = /ab+c/gi; const c = structuredClone(r); \
          return c instanceof RegExp && c.source === 'ab+c' && c.flags === 'gi'; })()"
    ));
    // Boolean/Number/String wrapper objects.
    assert!(h.eval_bool(
        "(() => { const c = structuredClone(new Number(7)); \
          return typeof c === 'object' && c.valueOf() === 7; })()"
    ));
}

#[test]
fn structured_clone_typed_arrays_share_buffer() {
    let h = Harness::new(PAGE);
    // Two views over one ArrayBuffer: the clones share one cloned buffer.
    assert!(h.eval_bool(
        "(() => { const buf = new ArrayBuffer(8); \
          const a = new Uint8Array(buf); const b = new Uint8Array(buf); \
          a[0] = 9; const c = structuredClone({a, b}); \
          return c.a.buffer === c.b.buffer && c.a.buffer !== buf && c.b[0] === 9; })()"
    ));
    // DataView clones over its (cloned) buffer.
    assert!(h.eval_bool(
        "(() => { const buf = new ArrayBuffer(4); const dv = new DataView(buf); \
          dv.setInt32(0, 123); const c = structuredClone(dv); \
          return c instanceof DataView && c.getInt32(0) === 123 && c.buffer !== buf; })()"
    ));
}

#[test]
fn structured_clone_errors() {
    let h = Harness::new(PAGE);
    // Error subclass: type, message, and cause preserved.
    assert!(h.eval_bool(
        "(() => { const e = new TypeError('bad', {cause: {why: 1}}); \
          const c = structuredClone(e); \
          return c instanceof TypeError && c.message === 'bad' && c.cause.why === 1; })()"
    ));
    // DOMException preserved.
    assert!(h.eval_bool(
        "(() => { const e = new DOMException('m', 'AbortError'); const c = structuredClone(e); \
          return c instanceof DOMException && c.name === 'AbortError' && c.message === 'm'; })()"
    ));
}

#[test]
fn structured_clone_rejects_uncloneable() {
    let h = Harness::new(PAGE);
    // Functions and symbols cannot be cloned.
    assert_eq!(threw(&h, "structuredClone(() => {})"), "DOMException");
    assert_eq!(threw(&h, "structuredClone(Symbol())"), "DOMException");
    // Host objects (DOM nodes) have an unrecognized prototype.
    assert_eq!(threw(&h, "structuredClone(document.body)"), "DOMException");
    // A non-empty transfer list is unsupported.
    assert_eq!(
        threw(&h, "structuredClone({}, {transfer: [new ArrayBuffer(1)]})"),
        "DOMException"
    );
    // An empty transfer list is fine.
    assert!(h.eval_bool(
        "(() => { const c = structuredClone({x: 1}, {transfer: []}); return c.x === 1; })()"
    ));
    // The thrown error is specifically a DataCloneError.
    assert_eq!(
        h.eval_string(
            "(() => { try { structuredClone(() => {}); return ''; } catch (e) { return e.name; } })()"
        ),
        "DataCloneError"
    );
}

// === performance.timing + user timing ===

#[test]
fn performance_timing_is_same_object_and_zero_before_milestones() {
    let h = Harness::new(PAGE);
    assert!(h.eval_bool("performance.timing instanceof PerformanceTiming"));
    // [SameObject]: repeated reads return the identical object.
    assert!(h.eval_bool("performance.timing === performance.timing"));
    // No milestone has been recorded in this bare bindings harness (no page
    // navigation), so every field reads 0.
    assert!(h.eval_bool("performance.timing.navigationStart === 0"));
    assert!(h.eval_bool("performance.timing.loadEventEnd === 0"));
    // unload*/redirect*/secureConnectionStart are hardcoded 0.
    assert!(h.eval_bool(
        "performance.timing.unloadEventStart === 0 \
          && performance.timing.redirectStart === 0 \
          && performance.timing.secureConnectionStart === 0"
    ));
}

#[test]
fn performance_user_timing_mark_and_measure() {
    let h = Harness::new(PAGE);
    // mark/measure produce entries readable via getEntriesByType/Name.
    assert!(h.eval_bool(
        "(() => { \
          performance.mark('a'); performance.mark('b'); \
          performance.measure('a-to-b', 'a', 'b'); \
          const marks = performance.getEntriesByType('mark'); \
          const measures = performance.getEntriesByType('measure'); \
          return marks.length === 2 && marks[0].name === 'a' \
            && measures.length === 1 && measures[0].name === 'a-to-b' \
            && measures[0].entryType === 'measure'; })()"
    ));
    // getEntriesByName filters by name; a fresh array is returned each call.
    assert!(h.eval_bool(
        "(() => { performance.mark('x'); \
          const a = performance.getEntriesByName('x'); \
          const b = performance.getEntriesByName('x'); \
          return a.length === 1 && a !== b; })()"
    ));
    // Unknown entry types (navigation/resource/paint) read back empty.
    assert!(h.eval_bool("performance.getEntriesByType('navigation').length === 0"));
    assert!(h.eval_bool("performance.getEntriesByType('resource').length === 0"));
    // clearMarks removes marks.
    assert!(h.eval_bool(
        "(() => { performance.mark('gone'); performance.clearMarks('gone'); \
          return performance.getEntriesByName('gone').length === 0; })()"
    ));
}

// === ResizeObserver (construction / brand) ===

#[test]
fn resize_observer_construction_and_brand() {
    let h = Harness::new(PAGE);
    // Constructor requires a callback function.
    assert_eq!(threw(&h, "new ResizeObserver()"), "TypeError");
    assert_eq!(threw(&h, "new ResizeObserver(42)"), "TypeError");
    // A valid observer is branded and exposes the methods.
    assert!(h.eval_bool("new ResizeObserver(() => {}) instanceof ResizeObserver"));
    assert!(h.eval_bool(
        "(() => { const ro = new ResizeObserver(() => {}); \
          return typeof ro.observe === 'function' \
            && typeof ro.unobserve === 'function' \
            && typeof ro.disconnect === 'function'; })()"
    ));
    // observe requires an Element.
    assert_eq!(
        threw(&h, "new ResizeObserver(() => {}).observe('nope')"),
        "TypeError"
    );
    // Entry interface exists for polyfill feature-detection.
    assert!(h.eval_bool("typeof ResizeObserverEntry === 'function'"));
    // ResizeObserver is an illegal constructor for its entry.
    assert_eq!(threw(&h, "new ResizeObserverEntry()"), "TypeError");
}

// === IntersectionObserver (construction / parsing) ===

#[test]
fn intersection_observer_construction_and_init() {
    let h = Harness::new(PAGE);
    assert_eq!(threw(&h, "new IntersectionObserver()"), "TypeError");
    assert_eq!(threw(&h, "new IntersectionObserver(42)"), "TypeError");
    assert!(h.eval_bool("new IntersectionObserver(() => {}) instanceof IntersectionObserver"));
    // Defaults: viewport root (null), rootMargin "0px 0px 0px 0px", thresholds [0].
    assert!(h.eval_bool("new IntersectionObserver(() => {}).root === null"));
    assert_eq!(
        h.eval_string("new IntersectionObserver(() => {}).rootMargin"),
        "0px 0px 0px 0px"
    );
    assert!(h.eval_bool(
        "(() => { const io = new IntersectionObserver(() => {}); \
          return io.thresholds.length === 1 && io.thresholds[0] === 0 \
            && Object.isFrozen(io.thresholds); })()"
    ));
    // threshold array is sorted; rootMargin shorthand expands.
    assert!(h.eval_bool(
        "(() => { const io = new IntersectionObserver(() => {}, \
            {threshold: [1, 0, 0.5], rootMargin: '10px 20%'}); \
          return io.thresholds.join(',') === '0,0.5,1' \
            && io.rootMargin === '10px 20% 10px 20%'; })()"
    ));
    // An out-of-range threshold is a RangeError.
    assert_eq!(
        threw(&h, "new IntersectionObserver(() => {}, {threshold: 1.5})"),
        "RangeError"
    );
    assert_eq!(
        threw(
            &h,
            "new IntersectionObserver(() => {}, {threshold: [-0.1]})"
        ),
        "RangeError"
    );
    // A malformed rootMargin is a SyntaxError DOMException.
    assert_eq!(
        h.eval_string(
            "(() => { try { new IntersectionObserver(() => {}, {rootMargin: 'garbage'}); return ''; } \
              catch (e) { return e.name; } })()"
        ),
        "SyntaxError"
    );
    // A Document root behaves like the viewport (null).
    assert!(h.eval_bool("new IntersectionObserver(() => {}, {root: document}).root === null"));
    // takeRecords returns an array (empty before any observation).
    assert!(h.eval_bool(
        "(() => { const io = new IntersectionObserver(() => {}); \
          return Array.isArray(io.takeRecords()) && io.takeRecords().length === 0; })()"
    ));
    // Entry interface exists for feature detection.
    assert!(h.eval_bool("'isIntersecting' in IntersectionObserverEntry.prototype"));
    assert!(h.eval_bool("'intersectionRatio' in IntersectionObserverEntry.prototype"));
}

// === Code-review regressions (2026-07-11) ===

#[test]
fn structured_clone_clones_builtin_subclasses_and_array_extra_keys() {
    let h = Harness::new(PAGE);
    // A Map subclass clones to a base Map (spec clones via internal slots).
    assert!(h.eval_bool(
        "(() => { class MyMap extends Map {} const c = structuredClone(new MyMap([['k', 1]])); \
          return c instanceof Map && !(c instanceof MyMap) && c.get('k') === 1; })()"
    ));
    // A Date subclass likewise.
    assert!(h.eval_bool(
        "(() => { class MyDate extends Date {} const c = structuredClone(new MyDate(7)); \
          return c instanceof Date && c.getTime() === 7; })()"
    ));
    // Arrays carry their non-index own enumerable string keys.
    assert!(h.eval_bool(
        "(() => { const a = [1, 2]; a.foo = 'bar'; const c = structuredClone(a); \
          return c.length === 2 && c[0] === 1 && c.foo === 'bar'; })()"
    ));
}

#[test]
fn performance_measure_throws_on_unknown_mark() {
    let h = Harness::new(PAGE);
    // A typo'd start mark is a SyntaxError, not a silent 0-start measure.
    assert_eq!(
        h.eval_string(
            "(() => { try { performance.measure('m', 'no-such-mark'); return ''; } \
              catch (e) { return e.name; } })()"
        ),
        "SyntaxError"
    );
    // A real mark still resolves.
    assert!(h.eval_bool(
        "(() => { performance.mark('a'); const m = performance.measure('m', 'a'); \
          return m.entryType === 'measure' && m.duration >= 0; })()"
    ));
}

#[test]
fn intersection_observer_rejects_invalid_root() {
    let h = Harness::new(PAGE);
    // A non-Element, non-Document root is a TypeError.
    assert_eq!(
        threw(&h, "new IntersectionObserver(() => {}, {root: 'scroller'})"),
        "TypeError"
    );
    assert_eq!(
        threw(
            &h,
            "new IntersectionObserver(() => {}, {root: document.createTextNode('t')})"
        ),
        "TypeError"
    );
    // An Element root is accepted and returned.
    assert!(h.eval_bool(
        "(() => { const el = document.getElementById('main'); \
          return new IntersectionObserver(() => {}, {root: el}).root === el; })()"
    ));
}

// === Web platform APIs added to unblock Angular/SPA bootstrap (ADR-0012) ===

#[test]
fn crypto_get_random_values_and_uuid() {
    let h = Harness::new(PAGE);
    assert_eq!(h.eval_string("typeof crypto.getRandomValues"), "function");
    // getRandomValues fills the view and returns it.
    assert!(h.eval_bool(
        "(() => { const a = new Uint8Array(16); return crypto.getRandomValues(a) === a; })()"
    ));
    // A v4 UUID: 36 chars, version nibble 4, variant in [89ab].
    assert!(h.eval_bool(
        "(() => { const u = crypto.randomUUID(); \
          return u.length === 36 && u[14] === '4' && '89ab'.includes(u[19]); })()"
    ));
    // Two UUIDs differ.
    assert!(h.eval_bool("crypto.randomUUID() !== crypto.randomUUID()"));
    // Float typed arrays are rejected.
    assert_eq!(
        threw(&h, "crypto.getRandomValues(new Float32Array(4))"),
        "DOMException"
    );
}

#[test]
fn text_encoder_decoder_roundtrip() {
    let h = Harness::new(PAGE);
    // Multi-byte + astral roundtrip.
    assert!(h.eval_bool(
        "(() => { const s = 'a\\u00e9\\u4e16\\ud83d\\ude00'; \
          const b = new TextEncoder().encode(s); \
          return b instanceof Uint8Array && new TextDecoder().decode(b) === s; })()"
    ));
    // Known UTF-8 length: 'a'(1) + é(2) + 世(3) + 😀(4) = 10 bytes.
    assert_eq!(
        h.eval_number("new TextEncoder().encode('a\\u00e9\\u4e16\\ud83d\\ude00').length"),
        10.0
    );
    // Invalid lead byte decodes to the replacement character.
    assert_eq!(
        h.eval_string("new TextDecoder().decode(new Uint8Array([0xff, 0x41]))"),
        "\u{fffd}A"
    );
    assert_eq!(h.eval_string("new TextDecoder().encoding"), "utf-8");
}

#[test]
fn web_storage_named_access_and_isolation() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => { localStorage.setItem('k','v'); localStorage.foo='bar'; \
              return localStorage.getItem('k') + '/' + localStorage.foo \
                + '/' + localStorage.length + '/' + Object.keys(localStorage).sort().join(','); })()"
        ),
        "v/bar/2/foo,k"
    );
    // removeItem + separate session storage.
    assert!(h.eval_bool(
        "(() => { localStorage.setItem('x','1'); localStorage.removeItem('x'); \
          return localStorage.getItem('x') === null \
            && sessionStorage !== localStorage && sessionStorage.length === 0; })()"
    ));
}

/// The storage objects must be real `Storage` instances: VueUse's `useStorage`
/// does `storage instanceof Storage` unguarded, and a bare object turns that
/// into a ReferenceError that rejects the whole app's bootstrap.
#[test]
fn storage_is_a_storage_instance_with_prototype_members() {
    let h = Harness::new(PAGE);
    assert!(h.eval_bool("localStorage instanceof Storage && sessionStorage instanceof Storage"));
    assert!(h.eval_bool("typeof Storage.prototype.setItem === 'function'"));
    // Direct construction is illegal, as in browsers.
    assert!(h.eval_bool(
        "(() => { try { new Storage(); return false; } catch (e) { return e instanceof TypeError; } })()"
    ));
    // Members are reached through the prototype, so patching it is observable —
    // analytics libraries do exactly this.
    assert_eq!(
        h.eval_string(
            "(() => { const original = Storage.prototype.setItem; \
              const seen = []; \
              Storage.prototype.setItem = function (k, v) { seen.push(k); return original.call(this, k, v); }; \
              localStorage.setItem('patched', '1'); \
              Storage.prototype.setItem = original; \
              return seen.join(',') + '/' + localStorage.getItem('patched'); })()"
        ),
        "patched/1"
    );
    // No `[LegacyOverrideBuiltIns]`: a stored key never shadows a member.
    assert!(h.eval_bool(
        "(() => { localStorage.clear(); localStorage.setItem('length', 'shadow'); \
          return localStorage.length === 1 && localStorage.getItem('length') === 'shadow'; })()"
    ));
    // Illegal invocation on a foreign receiver.
    assert!(h.eval_bool(
        "(() => { try { Storage.prototype.getItem.call({}, 'k'); return false; } \
                  catch (e) { return e instanceof TypeError; } })()"
    ));
}

/// Script mints `StorageEvent`s itself (VueUse does, on every write). The
/// engine never *fires* one — a storage event notifies the other documents of
/// the origin, and there are none — but the constructor must exist.
#[test]
fn storage_event_is_constructible() {
    let h = Harness::new(PAGE);
    assert!(h.eval_bool(
        "(() => { const e = new StorageEvent('storage', \
            { key: 'k', oldValue: null, newValue: 'v', storageArea: localStorage }); \
          return e instanceof StorageEvent && e instanceof Event && e.type === 'storage' \
            && e.key === 'k' && e.oldValue === null && e.newValue === 'v' \
            && e.url === '' && e.storageArea === localStorage; })()"
    ));
}

/// Vue's `createApp().mount()` brand-checks the mount target with
/// `el instanceof SVGElement`, unguarded, and VitePress reads link targets with
/// `e.href instanceof SVGAnimatedString ? e.href.animVal : e.href`.
#[test]
fn svg_elements_and_animated_href() {
    let h = Harness::new(
        "<!DOCTYPE html><html><body><svg xmlns:xlink='http://www.w3.org/1999/xlink'>\
         <a id='a' href='/one'><rect id='r'/></a><a id='legacy' xlink:href='/old'></a>\
         </svg></body></html>",
    );
    assert!(h.eval_bool("document.querySelector('svg') instanceof SVGElement"));
    // Elements without their own interface land on the base one.
    assert!(h.eval_bool(
        "(() => { const r = document.getElementById('r'); \
          return r instanceof SVGElement && !(r instanceof SVGAElement); })()"
    ));
    // The SVG `<a>` is not an HTMLAnchorElement, and its href is not a string.
    assert!(h.eval_bool(
        "(() => { const a = document.getElementById('a'); \
          return a instanceof SVGAElement && a instanceof SVGElement \
            && !(a instanceof HTMLAnchorElement) \
            && a.href instanceof SVGAnimatedString && a.href === a.href; })()"
    ));
    assert_eq!(
        h.eval_string("document.getElementById('a').href.baseVal"),
        "/one"
    );
    assert_eq!(
        h.eval_string("document.getElementById('a').href.animVal"),
        "/one"
    );
    // `xlink:href` is the SVG 1.1 spelling, read as a fallback.
    assert_eq!(
        h.eval_string("document.getElementById('legacy').href.baseVal"),
        "/old"
    );
    // baseVal writes through to the SVG 2 content attribute.
    assert_eq!(
        h.eval_string(
            "(() => { const a = document.getElementById('a'); a.href.baseVal = '/two'; \
              return a.getAttribute('href') + '|' + a.href.animVal; })()"
        ),
        "/two|/two"
    );
    // An HTML anchor's href stays a plain string.
    assert!(h.eval_bool("!(document.createElement('a').href instanceof SVGAnimatedString)"));
}

#[test]
fn request_idle_callback_shims_exist() {
    let h = Harness::new(PAGE);
    assert_eq!(h.eval_string("typeof requestIdleCallback"), "function");
    assert_eq!(h.eval_string("typeof cancelIdleCallback"), "function");
    assert!(h.eval_bool("typeof requestIdleCallback(() => {}) === 'number'"));
}

#[test]
fn history_state_stack() {
    let h = Harness::new(PAGE);
    // State-only push/replace (no URL) is origin-independent.
    assert_eq!(
        h.eval_string(
            "(() => { const n0 = history.length; \
              history.pushState({a:1}, ''); \
              const s1 = JSON.stringify(history.state); \
              history.replaceState({a:2}, ''); \
              return s1 + '|' + JSON.stringify(history.state) + '|' + (history.length - n0); })()"
        ),
        "{\"a\":1}|{\"a\":2}|1"
    );
    assert_eq!(h.eval_string("history.scrollRestoration"), "auto");
    // A real IDL interface now (ADR-0022), not a closure shim: it has a
    // prototype chain, a brand, and a `Symbol.toStringTag`.
    assert!(h.eval_bool("history instanceof History"));
    assert_eq!(
        h.eval_string("Object.prototype.toString.call(history)"),
        "[object History]"
    );
    assert!(h.eval_bool("location instanceof Location"));
    // The state is a structured *clone*: mutating the object afterwards must
    // not reach into the entry.
    assert_eq!(
        h.eval_string(
            "(() => { const o = {v: 1}; history.replaceState(o, ''); o.v = 2; \
              return String(history.state.v); })()"
        ),
        "1"
    );
    // `pushState` to another origin is a SecurityError, and the document URL
    // must not have moved.
    assert_eq!(
        h.eval_string(
            "(() => { const before = location.href; \
              try { history.pushState(null, '', 'https://evil.example/x'); return 'no throw'; } \
              catch (e) { return e.name + '|' + String(location.href === before); } })()"
        ),
        "SecurityError|true"
    );
}

#[test]
fn node_iterator_filters_comments_like_angular() {
    let h = Harness::new(PAGE);
    // Angular hydration walks SHOW_COMMENT with an acceptNode filter.
    assert_eq!(
        h.eval_string(
            "(() => { const host = document.getElementById('main'); \
              host.innerHTML = '<!--ngetn-->A<span>B</span><!--x--><!--ngtns-->'; \
              const it = document.createNodeIterator(host, NodeFilter.SHOW_COMMENT, { \
                acceptNode(n) { const t = n.textContent; \
                  return (t === 'ngetn' || t === 'ngtns') \
                    ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_REJECT; } }); \
              const out = []; let c; while ((c = it.nextNode())) out.push(c.textContent); \
              return out.join(','); })()"
        ),
        "ngetn,ngtns"
    );
    assert_eq!(threw(&h, "new NodeIterator()"), "TypeError");
}

#[test]
fn tree_walker_walks_elements() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => { const host = document.getElementById('main'); \
              const w = document.createTreeWalker(host, NodeFilter.SHOW_ELEMENT); \
              const out = []; let n; while ((n = w.nextNode())) out.push(n.tagName); \
              return out.join(','); })()"
        ),
        "P,P,SPAN"
    );
}

#[test]
fn set_attribute_ns_namespaced() {
    let h = Harness::new(PAGE);
    let xlink = "http://www.w3.org/1999/xlink";
    assert_eq!(
        h.eval_string(&format!(
            "(() => {{ const use = document.createElementNS('http://www.w3.org/2000/svg','use'); \
              use.setAttributeNS('{xlink}','xlink:href','#icon'); \
              const got = use.getAttributeNS('{xlink}','href'); \
              use.setAttributeNS('{xlink}','xlink:href','#icon2'); \
              const n = use.attributes.length; \
              use.removeAttributeNS('{xlink}','href'); \
              return got + '/' + n + '/' + use.hasAttributeNS('{xlink}','href'); }})()"
        )),
        "#icon/1/false"
    );
    // A prefixed name with a null namespace is a NamespaceError.
    assert_eq!(
        h.eval_string(
            "(() => { const e = document.createElement('div'); \
              try { e.setAttributeNS(null, 'x:y', '1'); return 'NO THROW'; } \
              catch (err) { return err.name; } })()"
        ),
        "NamespaceError"
    );
}

#[test]
fn dom_parser_and_implementation_inert_documents() {
    let h = Harness::new(PAGE);
    // document.implementation.createHTMLDocument yields a Document with a body.
    assert_eq!(
        h.eval_string(
            "(() => { const d = document.implementation.createHTMLDocument('t'); \
              d.body.innerHTML = '<p>hi</p>'; \
              return d.body.tagName + '/' + d.title + '/' + d.nodeType \
                + '/' + d.body.firstChild.tagName; })()"
        ),
        "BODY/t/9/P"
    );
    // DOMParser parses body-level markup (Angular sanitizer pattern).
    assert_eq!(
        h.eval_string(
            "(() => { const doc = new DOMParser() \
                .parseFromString('<body><remove></remove><p>safe</p>', 'text/html'); \
              const b = doc.body; b.firstChild.remove(); \
              return b.firstChild.tagName + '/' + b.textContent; })()"
        ),
        "P/safe"
    );
}

/// The second document is a *real* Document (ADR-0017), so a full-document
/// parse puts head content in `<head>` — the ADR-0012 "head-only content is
/// approximate" limit is gone — and the result is isolated from the page.
#[test]
fn dom_parser_parses_a_whole_document_into_a_real_document() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => { const d = new DOMParser().parseFromString( \
                '<!doctype html><html><head><title>T</title>\
                 <meta name=x content=y></head><body><p>hi</p></body></html>', 'text/html'); \
              return [d.doctype.name, d.title, d.head.children.length, \
                      !!d.head.querySelector('meta[name=x]'), d.body.textContent, \
                      d.defaultView === null, d.location === null, \
                      d.body.ownerDocument === d, \
                      document.contains(d.body)].join('/'); })()"
        ),
        "html/T/2/true/hi/true/true/true/false"
    );
    // An unsupported type is a TypeError, not a DOMException.
    assert_eq!(
        h.eval_string(
            "(() => { try { new DOMParser().parseFromString('x', 'text/plain'); return 'no throw'; } \
              catch (e) { return e.constructor.name; } })()"
        ),
        "TypeError"
    );
}

/// A node keeps its node document alive and reachable: `createElement` on a
/// second document must not silently land the node in the page's document, and
/// scripts of an inert document never run.
#[test]
fn second_document_owns_its_nodes_and_stays_inert() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => { globalThis.ran = false; \
              const d = new DOMParser().parseFromString( \
                '<body><style>p{color:red}</style>\
                 <script>globalThis.ran = true;<\\/script><p id=q>x</p></body>', 'text/html'); \
              const el = d.createElement('div'); \
              return [el.ownerDocument === d, el.ownerDocument !== document, \
                      d.getElementById('q').ownerDocument === d, \
                      globalThis.ran, d.styleSheets.length, \
                      document.styleSheets.length].join('/'); })()"
        ),
        "true/true/true/false/0/0"
    );
    // `new Document()` is an XML document: no lowercasing, null namespace, and
    // CDATA sections are allowed (they are not, in an HTML document).
    assert_eq!(
        h.eval_string(
            "(() => { const x = new Document(); \
              const el = x.createElement('DIV'); \
              let htmlThrew = ''; \
              try { document.createCDATASection('x'); } catch (e) { htmlThrew = e.name; } \
              return [el.localName, el.namespaceURI === null, \
                      el.constructor.name, x.contentType, \
                      x.createCDATASection('a').nodeType, htmlThrew].join('/'); })()"
        ),
        "DIV/true/Element/application/xml/4/NotSupportedError"
    );
}

/// The mutation-observer compound microtask is queued when the record is
/// queued, so it is *ordered against* promise reactions rather than draining
/// after all of them: `await Promise.resolve()` later in the same task must not
/// overtake it. Delivering observers only after `pump_jobs()` had emptied the
/// job queue inverted this.
#[test]
fn mutation_observer_delivery_is_a_microtask_ordered_with_promises() {
    let h = Harness::new(PAGE);
    // The observer must run *between* the synchronous mutation and a promise
    // reaction queued after it — not after the whole job queue has drained.
    h.eval(
        "globalThis.log = []; \
         (() => { const el = document.createElement('div'); \
           new MutationObserver(() => log.push('observer')).observe(el, {childList: true}); \
           el.textContent = 'x'; \
           log.push('sync'); \
           Promise.resolve().then(() => log.push('promise')); })();",
    )
    .expect("eval");
    assert_eq!(h.eval_string("log.join(',')"), "sync,observer,promise");

    // "Replace all" is one operation: one record naming the removal and the
    // addition together, not one record per child plus one for the insert.
    h.eval(
        "globalThis.seen = null; \
         (() => { const el = document.createElement('div'); \
           el.appendChild(document.createTextNode('foo')); \
           new MutationObserver(r => { seen = r.length + '/' + r[0].removedNodes.length \
             + '/' + r[0].addedNodes.length; }).observe(el, {childList: true}); \
           el.textContent = 'bar'; })();",
    )
    .expect("eval");
    assert_eq!(h.eval_string("seen"), "1/1/1");
}

/// `splitText` on a *detached* node of a second document: the new node must
/// stay in that document (no insertion will come along to adopt it), and
/// splitting a CDATASection yields a CDATASection, not a Text.
#[test]
fn split_text_keeps_its_document_and_its_kind() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => { const x = new Document(); \
              const c = x.createCDATASection('abcdef'); \
              const tail = c.splitText(3); \
              const t = x.createTextNode('xyzw'); \
              const t2 = t.splitText(2); \
              return [c.data, tail.data, tail.nodeType, tail.ownerDocument === x, \
                      t2.nodeType, t2.ownerDocument === x].join('/'); })()"
        ),
        "abc/def/4/true/3/true"
    );
}

/// `importNode` clones into the receiving document; `adoptNode` moves the node
/// and its whole subtree, which is no longer "just removal".
#[test]
fn import_and_adopt_are_real_cross_document_operations() {
    let h = Harness::new(PAGE);
    assert_eq!(
        h.eval_string(
            "(() => { const d = document.implementation.createHTMLDocument('t'); \
              const src = document.createElement('div'); \
              src.appendChild(document.createElement('span')); \
              document.body.appendChild(src); \
              const imported = d.importNode(src, true); \
              const adopted = d.adoptNode(src); \
              return [imported.ownerDocument === d, \
                      imported.firstChild.ownerDocument === d, \
                      src.parentNode === null, \
                      adopted === src, \
                      adopted.ownerDocument === d, \
                      adopted.firstChild.ownerDocument === d, \
                      adopted.isConnected].join('/'); })()"
        ),
        "true/true/true/true/true/true/false"
    );
}

#[test]
fn readable_stream_and_response_body_present() {
    let h = Harness::new(PAGE);
    assert_eq!(h.eval_string("typeof ReadableStream"), "function");
    // Response.body is a readable byte stream (Angular FetchBackend reads it).
    assert!(h.eval_bool(
        "(() => { const b = new Response('hi').body; \
          return b !== null && typeof b.getReader === 'function' \
            && typeof b.getReader().read === 'function'; })()"
    ));
    // A hand-built ReadableStream exposes the reader surface.
    assert!(h.eval_bool(
        "(() => { const s = new ReadableStream({ start(c) { c.enqueue(1); c.close(); } }); \
          return typeof s.getReader().read === 'function'; })()"
    ));
}

// === Event handler content attributes (`<div onclick="…">`) ===

/// A document whose markup declares handlers, as WPT's `check-layout-th.js`
/// tests do (`<body onload="checkLayout(…)">`).
///
/// The `<script>` elements carry the only event handlers this engine also
/// declares as *IDL* attributes (`onload`/`onerror`), so they are what exercise
/// the interplay between a declared handler and an assigned one. `onclick` has
/// no IDL attribute here, and reaches its element through the content attribute
/// alone.
const HANDLER_PAGE: &str = r#"<!DOCTYPE html>
<html><head><title>Handlers</title></head>
<body>
  <div id="declared" onclick="globalThis.seen = this.id + ':' + event.type"></div>
  <div id="scoped" onclick="globalThis.scope = typeof createElement"></div>
  <div id="plain" onfoo="globalThis.foo = 1"></div>
  <script id="script" onload="globalThis.seen = 'declared'"></script>
  <script id="broken" onload="this is not javascript"></script>
</body></html>"#;

fn click(h: &Harness, id: &str) {
    h.eval(&format!(
        "document.getElementById('{id}').dispatchEvent(new Event('click'))"
    ))
    .expect("dispatch");
}

fn fire_load(h: &Harness, id: &str) {
    h.eval(&format!(
        "document.getElementById('{id}').dispatchEvent(new Event('load'))"
    ))
    .expect("dispatch");
}

#[test]
fn a_declared_handler_runs_with_the_element_as_this() {
    let h = Harness::new(HANDLER_PAGE);
    click(&h, "declared");
    assert_eq!(h.eval_string("globalThis.seen"), "declared:click");
}

#[test]
fn a_declared_handler_reads_back_through_the_idl_attribute() {
    let h = Harness::new(HANDLER_PAGE);
    // The compiled function is what script sees, so a declared handler and an
    // assigned one are indistinguishable afterwards.
    assert_eq!(
        h.eval_string("typeof document.getElementById('script').onload"),
        "function"
    );
    // An element with no handler declared still reads back as null.
    assert_eq!(
        h.eval_string("typeof document.getElementById('broken').onerror"),
        "object"
    );
}

#[test]
fn a_declared_handler_resolves_names_against_the_document_then_the_global() {
    // The spec scope chain (element, then document, then global) is what lets
    // `<body onload="checkLayout('.flexbox')">` reach a global function.
    let h = Harness::new(HANDLER_PAGE);
    click(&h, "scoped");
    assert_eq!(h.eval_string("globalThis.scope"), "function");
}

#[test]
fn changing_the_content_attribute_replaces_the_handler() {
    let h = Harness::new(HANDLER_PAGE);
    click(&h, "declared");
    assert_eq!(h.eval_string("globalThis.seen"), "declared:click");

    h.eval("document.getElementById('declared').setAttribute('onclick', \"globalThis.seen = 'rewritten'\")")
        .expect("setAttribute");
    click(&h, "declared");
    assert_eq!(h.eval_string("globalThis.seen"), "rewritten");

    h.eval(
        "globalThis.seen = 'none'; document.getElementById('declared').removeAttribute('onclick')",
    )
    .expect("removeAttribute");
    click(&h, "declared");
    assert_eq!(h.eval_string("globalThis.seen"), "none");
}

#[test]
fn an_assigned_handler_supersedes_the_attribute_until_the_attribute_changes() {
    let h = Harness::new(HANDLER_PAGE);
    fire_load(&h, "script");
    assert_eq!(h.eval_string("globalThis.seen"), "declared");

    // Assigning through the IDL attribute wins, even though the content
    // attribute still says otherwise.
    h.eval("document.getElementById('script').onload = () => { globalThis.seen = 'assigned'; }")
        .expect("assign");
    fire_load(&h, "script");
    assert_eq!(h.eval_string("globalThis.seen"), "assigned");

    // A later edit of the attribute wins it back.
    h.eval(
        "document.getElementById('script').setAttribute('onload', \"globalThis.seen = 'reclaimed'\")",
    )
    .expect("setAttribute");
    fire_load(&h, "script");
    assert_eq!(h.eval_string("globalThis.seen"), "reclaimed");

    // Assigning null disables the handler although the attribute remains.
    h.eval("globalThis.seen = 'none'; document.getElementById('script').onload = null")
        .expect("assign null");
    fire_load(&h, "script");
    assert_eq!(h.eval_string("globalThis.seen"), "none");
    assert_eq!(
        h.eval_string("document.getElementById('script').getAttribute('onload')"),
        "globalThis.seen = 'reclaimed'"
    );

    // Removing the attribute clears the handler too.
    h.eval(
        "document.getElementById('script').setAttribute('onload', \"globalThis.seen = 'back'\")",
    )
    .expect("setAttribute");
    fire_load(&h, "script");
    assert_eq!(h.eval_string("globalThis.seen"), "back");
    h.eval("globalThis.seen = 'none'; document.getElementById('script').removeAttribute('onload')")
        .expect("removeAttribute");
    fire_load(&h, "script");
    assert_eq!(h.eval_string("globalThis.seen"), "none");
}

#[test]
fn a_handler_that_does_not_compile_is_reported_and_left_null() {
    let h = Harness::new(HANDLER_PAGE);
    // Dispatch must survive a syntax error in the attribute...
    fire_load(&h, "broken");
    assert_eq!(
        h.eval_string("typeof document.getElementById('broken').onload"),
        "object"
    );
    assert!(
        !h.hooks.errors.borrow().is_empty(),
        "the syntax error is reported"
    );
    // ...and it must not be recompiled (and re-reported) on every dispatch.
    let reported = h.hooks.errors.borrow().len();
    fire_load(&h, "broken");
    assert_eq!(h.hooks.errors.borrow().len(), reported);
}

#[test]
fn an_attribute_outside_the_handler_set_is_not_a_handler() {
    // `onfoo` is an ordinary attribute: compiling it would invent an API.
    let h = Harness::new(HANDLER_PAGE);
    h.eval("document.getElementById('plain').dispatchEvent(new Event('foo'))")
        .expect("dispatch");
    assert_eq!(h.eval_string("typeof globalThis.foo"), "undefined");
}

#[test]
fn unqualified_event_target_calls_receive_the_window_as_receiver() {
    // WebIDL substitutes the global for a null/undefined receiver. `testharness.js`
    // registers its error handler exactly this way — `addEventListener("error", …)`
    // with no receiver — and threw here, which aborted the harness mid-file and
    // left `output_handler` null, so every later `setup({…})` failed.
    let h = Harness::new(PAGE);
    assert!(h.eval_bool(
        "(() => { let seen = null; \
           addEventListener('probe', e => { seen = e.type; }); \
           dispatchEvent(new Event('probe')); \
           return seen === 'probe'; })()"
    ));
    // The listener really landed on the window, not on some other target.
    assert!(h.eval_bool(
        "(() => { let count = 0; \
           const fn = () => { count++; }; \
           addEventListener('probe2', fn); \
           window.dispatchEvent(new Event('probe2')); \
           removeEventListener('probe2', fn); \
           window.dispatchEvent(new Event('probe2')); \
           return count === 1; })()"
    ));
}
