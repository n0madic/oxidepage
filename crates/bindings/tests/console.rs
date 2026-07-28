//! ADR-0025: the console argument encoder and the console methods.
//!
//! No event loop is needed — the encoder runs entirely inside one host call —
//! so this uses the same realm-plus-`PageState` harness as `bindings.rs`.

use std::cell::RefCell;
use std::rc::Rc;

use oxidepage_base::{RequestId, id::FIRST_GENERATION};
use oxidepage_bindings::{
    BindCx, ConsoleLevel, ConsoleMessage, DialogRequest, DialogResponse, HostHooks,
    PREVIEW_MAX_ENTRIES, PREVIEW_MAX_STRING, PageState, ScriptError, ValuePreview, install,
};
use oxidepage_bindings::{PrivateStorageAreas, SharedStorage, StorageAreaKind};
use oxidepage_dom::{ParseOptions, parse_document};
use oxidepage_js::{JsEngine, JsRealm, JsValue, PromiseState, QuickJsEngine, RealmOptions};
use oxidepage_net::NetRequest;

#[derive(Default)]
struct Hooks {
    console: RefCell<Vec<ConsoleMessage>>,
    errors: RefCell<Vec<ScriptError>>,
    next_id: std::cell::Cell<u32>,
    storage: PrivateStorageAreas,
}

impl HostHooks for Hooks {
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
    fn run_dialog(&self, _request: DialogRequest) -> DialogResponse {
        DialogResponse::Dismiss
    }
    fn schedule_timer(&self, _c: JsValue, _a: Vec<JsValue>, _d: f64, _r: bool) -> f64 {
        0.0
    }
    fn clear_timer(&self, _id: f64) {}
    fn request_animation_frame(&self, _callback: JsValue) -> f64 {
        0.0
    }
    fn cancel_animation_frame(&self, _id: f64) {}
    fn start_fetch(&self, _request: NetRequest) -> RequestId {
        let n = self.next_id.get() + 1;
        self.next_id.set(n);
        RequestId::from_parts(n, FIRST_GENERATION)
    }
    fn abort(&self, _id: RequestId) {}
    fn get_cookie(&self, _document_url: &str) -> String {
        String::new()
    }
    fn set_cookie(&self, _document_url: &str, _cookie: &str) {}
}

struct Harness {
    // Field order = drop order: the state owns persistent JS references and
    // must drop before the realm.
    state: Rc<PageState>,
    hooks: Rc<Hooks>,
    realm: oxidepage_js::QuickJsRealm,
}

impl Harness {
    fn new() -> Self {
        Self::with_html("<html><body><div id='app' class='hero'>hi</div></body></html>")
    }

    fn with_html(html: &str) -> Self {
        let realm = QuickJsEngine
            .new_realm(RealmOptions::default())
            .expect("realm");
        let dom = Rc::new(RefCell::new(
            parse_document(html, ParseOptions::default()).tree,
        ));
        let hooks = Rc::new(Hooks::default());
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

    fn eval(&self, source: &str) {
        self.realm.with_scope(|scope| {
            let result = scope.eval(source, "http://x/test.js");
            let cx = BindCx {
                scope,
                state: Rc::clone(&self.state),
            };
            oxidepage_bindings::microtask_checkpoint(&cx);
            result.expect("eval");
        });
    }

    /// Logs `expression` and returns its single argument preview.
    fn preview(&self, expression: &str) -> ValuePreview {
        self.eval(&format!("console.log({expression})"));
        let console = self.hooks.console.borrow();
        let message = console.last().expect("a console message");
        assert_eq!(message.args.len(), 1, "expected one argument");
        message.args[0].clone()
    }

    /// Logs `expression` and returns the rendered line.
    fn rendered(&self, expression: &str) -> String {
        self.eval(&format!("console.log({expression})"));
        self.hooks.console.borrow().last().unwrap().message.clone()
    }

    fn messages(&self) -> Vec<ConsoleMessage> {
        self.hooks.console.borrow().clone()
    }
}

#[test]
fn primitives_encode_to_their_own_variants() {
    let h = Harness::new();
    assert_eq!(h.preview("undefined"), ValuePreview::Undefined);
    assert_eq!(h.preview("null"), ValuePreview::Null);
    assert_eq!(h.preview("true"), ValuePreview::Bool(true));
    assert_eq!(h.preview("1.5"), ValuePreview::Number(1.5));
    assert_eq!(
        h.preview("'text'"),
        ValuePreview::String {
            value: "text".into(),
            truncated: false
        }
    );
    assert_eq!(h.preview("123n"), ValuePreview::BigInt("123".into()));
    // `ToString` on a symbol throws — this is the case that used to print
    // `<unprintable>`.
    assert_eq!(
        h.preview("Symbol('sym')"),
        ValuePreview::Symbol("sym".into())
    );
    assert_eq!(h.preview("Symbol()"), ValuePreview::Symbol(String::new()));
}

#[test]
fn special_numbers_survive() {
    let h = Harness::new();
    assert_eq!(h.rendered("NaN"), "NaN");
    assert_eq!(h.rendered("Infinity"), "Infinity");
    assert_eq!(h.rendered("-Infinity"), "-Infinity");
    assert_eq!(h.rendered("-0"), "-0");
    assert_eq!(h.rendered("1e21"), "1e+21");
}

#[test]
fn functions_arrays_and_objects_encode_structurally() {
    let h = Harness::new();
    assert_eq!(
        h.preview("function foo(){}"),
        ValuePreview::Function { name: "foo".into() }
    );
    assert_eq!(
        h.preview("[1, 'a']"),
        ValuePreview::Array {
            items: vec![
                ValuePreview::Number(1.0),
                ValuePreview::String {
                    value: "a".into(),
                    truncated: false
                }
            ],
            length: 2,
            truncated: false,
        }
    );
    let ValuePreview::Object {
        class,
        entries,
        truncated,
        ..
    } = h.preview("({ a: 1, b: { c: 2 } })")
    else {
        panic!("expected an object preview");
    };
    assert_eq!(class, "Object");
    assert!(!truncated);
    assert_eq!(entries[0], ("a".into(), ValuePreview::Number(1.0)));
    assert!(matches!(entries[1].1, ValuePreview::Object { .. }));
}

#[test]
fn a_class_instance_reports_its_constructor_name() {
    let h = Harness::new();
    let preview = h.preview("new (class Widget { constructor(){ this.n = 1 } })()");
    let ValuePreview::Object { class, .. } = preview else {
        panic!("expected an object preview");
    };
    assert_eq!(class, "Widget");
    assert_eq!(
        h.rendered("new (class Widget { constructor(){ this.n = 1 } })()"),
        "Widget {n: 1}"
    );
}

#[test]
fn errors_carry_name_message_and_frames() {
    let h = Harness::new();
    h.eval("function boom(){ return new TypeError('bad') }\nconsole.log(boom());");
    let console = h.hooks.console.borrow();
    let ValuePreview::Error {
        name,
        message,
        stack,
    } = &console.last().unwrap().args[0]
    else {
        panic!("expected an error preview");
    };
    assert_eq!(name, "TypeError");
    assert_eq!(message, "bad");
    assert_eq!(stack.first().unwrap().function.as_deref(), Some("boom"));
    assert_eq!(stack.first().unwrap().url, "http://x/test.js");
}

#[test]
fn promise_state_is_reported() {
    let h = Harness::new();
    assert_eq!(
        h.preview("new Promise(() => {})"),
        ValuePreview::Promise {
            state: PromiseState::Pending
        }
    );
    assert_eq!(
        h.preview("Promise.resolve(1)"),
        ValuePreview::Promise {
            state: PromiseState::Fulfilled
        }
    );
}

#[test]
fn a_dom_element_is_named_not_walked() {
    let h = Harness::new();
    let preview = h.preview("document.getElementById('app')");
    let ValuePreview::Node { name, description } = preview else {
        panic!("expected a node preview, got {preview:?}");
    };
    assert_eq!(name, "DIV");
    assert_eq!(description, r#"<div id="app" class="hero">"#);
    // The document itself, and a text node.
    assert!(matches!(
        h.preview("document"),
        ValuePreview::Node { ref name, .. } if name == "#document"
    ));
    assert!(matches!(
        h.preview("document.getElementById('app').firstChild"),
        ValuePreview::Node { ref name, .. } if name == "#text"
    ));
}

/// The encoder describes a node from the tree, never by minting a wrapper —
/// so the snapshot is of the node *as it was*, and a later mutation cannot
/// rewrite the record.
#[test]
fn node_previews_are_snapshots() {
    let h = Harness::new();
    h.eval("console.log(document.getElementById('app'));");
    h.eval("document.getElementById('app').setAttribute('class', 'changed');");
    let ValuePreview::Node { description, .. } = &h.hooks.console.borrow()[0].args[0] else {
        panic!("expected a node preview");
    };
    assert_eq!(description, r#"<div id="app" class="hero">"#);
}

#[test]
fn the_depth_cap_elides_rather_than_lying() {
    let h = Harness::new();
    // Six levels deep; the cap is four.
    let mut preview = h.preview("({a:{a:{a:{a:{a:{a:1}}}}}})");
    let mut depth = 0;
    loop {
        match preview {
            ValuePreview::Object { entries, .. } => {
                depth += 1;
                preview = entries.into_iter().next().unwrap().1;
            }
            ValuePreview::Elided => break,
            other => panic!("expected Elided at the cap, got {other:?} after {depth} levels"),
        }
    }
    assert_eq!(depth, oxidepage_bindings::PREVIEW_MAX_DEPTH);
}

#[test]
fn the_breadth_cap_reports_the_real_length() {
    let h = Harness::new();
    let ValuePreview::Array {
        items,
        length,
        truncated,
    } = h.preview("Array.from({length: 250}, (_, i) => i)")
    else {
        panic!("expected an array preview");
    };
    assert_eq!(length, 250);
    assert_eq!(items.len(), PREVIEW_MAX_ENTRIES);
    assert!(truncated);
    assert!(
        h.rendered("Array.from({length: 250}, (_, i) => i)")
            .ends_with(", … 150 more]")
    );

    let ValuePreview::Object {
        entries, truncated, ..
    } = h.preview("Object.fromEntries(Array.from({length: 250}, (_, i) => ['k' + i, i]))")
    else {
        panic!("expected an object preview");
    };
    assert_eq!(entries.len(), PREVIEW_MAX_ENTRIES);
    assert!(truncated);
}

#[test]
fn long_strings_are_truncated_at_a_char_boundary() {
    let h = Harness::new();
    let ValuePreview::String { value, truncated } = h.preview("'é'.repeat(20000)") else {
        panic!("expected a string preview");
    };
    assert!(truncated);
    assert_eq!(value.chars().count(), PREVIEW_MAX_STRING);
}

#[test]
fn a_self_referential_object_terminates() {
    let h = Harness::new();
    let ValuePreview::Object { entries, .. } =
        h.preview("(() => { const a = {}; a.self = a; return a })()")
    else {
        panic!("expected an object preview");
    };
    assert_eq!(entries, vec![("self".to_owned(), ValuePreview::Cyclic)]);
    assert_eq!(
        h.rendered("(() => { const a = {}; a.self = a; return a })()"),
        "{self: [Circular]}"
    );
    // A repeated *sibling* is not a cycle: only a back-edge on the path is.
    assert_eq!(
        h.rendered("(() => { const x = {n: 1}; return {a: x, b: x} })()"),
        "{a: {n: 1}, b: {n: 1}}"
    );
    // A cycle through an array terminates too.
    h.eval("const arr = []; arr.push(arr); console.log(arr);");
    assert_eq!(h.messages().last().unwrap().message, "[[Circular]]");
}

/// The per-level caps alone allow 100^4 nodes, and cycle detection only
/// rejects a back-edge on the current *path* — so a shallow graph of shared
/// objects is re-walked exponentially. The total node budget is what makes the
/// cost flat, and nothing outside can rescue it (`ScriptBudget` is enforced
/// through the engine interrupt, which plain property reads never reach).
#[test]
fn a_shared_shallow_graph_cannot_blow_up() {
    let h = Harness::new();
    let started = std::time::Instant::now();
    h.eval(
        "const d = {}; for (let i = 0; i < 100; i++) d['k' + i] = i;
         const c = {}; for (let i = 0; i < 100; i++) c['k' + i] = d;
         const b = {}; for (let i = 0; i < 100; i++) b['k' + i] = c;
         const a = {}; for (let i = 0; i < 100; i++) a['k' + i] = b;
         console.log(a);",
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the encoder has no total node budget"
    );
    fn count(preview: &ValuePreview) -> usize {
        1 + match preview {
            ValuePreview::Object { entries, .. } => entries.iter().map(|(_, v)| count(v)).sum(),
            ValuePreview::Array { items, .. } => items.iter().map(count).sum(),
            _ => 0,
        }
    }
    let nodes = count(&h.hooks.console.borrow()[0].args[0]);
    assert!(
        nodes <= oxidepage_bindings::PREVIEW_MAX_NODES,
        "encoded {nodes} nodes, budget is {}",
        oxidepage_bindings::PREVIEW_MAX_NODES
    );
}

/// `Date`, `RegExp` and `URL` keep their content in internal slots, so an
/// enumerable-property walk sees nothing. Without a description they would
/// preview as `Date {}` — strictly less than the string rendering the preview
/// encoder replaced.
#[test]
fn exotic_built_ins_keep_their_string_form() {
    let h = Harness::new();
    // Not an exact match: `Date.prototype.toString` is local-time, and the
    // test must not depend on the machine's zone.
    let date = h.rendered("new Date(0)");
    assert!(
        date.starts_with("Date Thu Jan 01 1970") || date.starts_with("Date Wed Dec 31 1969"),
        "got {date}"
    );
    assert_eq!(h.rendered("/ab+c/gi"), "RegExp /ab+c/gi");
    assert_eq!(h.rendered("new URL('http://x/y')"), "URL http://x/y");
    // A plain object's `toString` is `Object.prototype`'s, so there is no
    // description to add and nothing changes.
    assert_eq!(h.rendered("({a: 1})"), "{a: 1}");
    // An exotic object that *also* has own properties shows both.
    assert_eq!(
        h.rendered("Object.assign(/x/, {tag: 1})"),
        "RegExp /x/ {tag: 1}"
    );
}

#[test]
fn a_throwing_getter_is_contained() {
    let h = Harness::new();
    let ValuePreview::Object { entries, .. } =
        h.preview("({ get boom(){ throw new Error('nope') } })")
    else {
        panic!("expected an object preview");
    };
    assert!(
        matches!(&entries[0].1, ValuePreview::Threw { message } if message.contains("nope")),
        "got {entries:?}"
    );
    // Previewing must not manufacture a page error of its own.
    assert!(h.hooks.errors.borrow().is_empty());
}

#[test]
fn objects_no_longer_render_as_object_object() {
    let h = Harness::new();
    assert_eq!(h.rendered("({a: 1})"), "{a: 1}");
    assert_eq!(h.rendered("[1, 2, 3]"), "[1, 2, 3]");
    assert_eq!(h.rendered("[{a: 1}]"), "[{a: 1}]");
    // A top-level string is unquoted; a nested one is quoted.
    assert_eq!(h.rendered("'plain'"), "plain");
    assert_eq!(h.rendered("['plain']"), r#"["plain"]"#);
}

#[test]
fn format_specifiers_are_applied() {
    let h = Harness::new();
    assert_eq!(h.rendered("'%s is %d', 'x', 4.7"), "x is 4");
    assert_eq!(h.rendered("'%i', '12abc'"), "NaN");
    assert_eq!(h.rendered("'%f', '1.5'"), "1.5");
    assert_eq!(h.rendered("'%o', {a: 1}"), "{a: 1}");
    assert_eq!(h.rendered("'%O', [1]"), "[1]");
    // The Formatter only runs with more than one argument (console spec's
    // Logger), so a lone string keeps its literal `%%`.
    assert_eq!(h.rendered("'100%% sure'"), "100%% sure");
    assert_eq!(h.rendered("'100%% sure', 'x'"), "100% sure x");
    // `%c` consumes its argument and emits nothing.
    assert_eq!(h.rendered("'%cred', 'color: red'"), "red");
    // Leftovers are appended, and a specifier with nothing left stays verbatim.
    assert_eq!(h.rendered("'%s', 'a', 'b', 1"), "a b 1");
    assert_eq!(h.rendered("'%s and %s', 'one'"), "one and %s");
    // Every raw argument is kept regardless of formatting.
    h.eval("console.log('%s', 'a', 'b')");
    assert_eq!(h.messages().last().unwrap().args.len(), 3);
}

#[test]
fn levels_map_to_the_methods() {
    let h = Harness::new();
    h.eval(
        "console.log('l'); console.info('i'); console.warn('w');
         console.error('e'); console.debug('d'); console.trace('t');",
    );
    let levels: Vec<_> = h.messages().iter().map(|m| m.level).collect();
    assert_eq!(
        levels,
        [
            ConsoleLevel::Log,
            ConsoleLevel::Info,
            ConsoleLevel::Warn,
            ConsoleLevel::Error,
            ConsoleLevel::Debug,
            ConsoleLevel::Trace,
        ]
    );
}

#[test]
fn assert_is_silent_when_the_condition_holds() {
    let h = Harness::new();
    h.eval("console.assert(true, 'never shown'); console.assert(1);");
    assert!(h.messages().is_empty());

    h.eval("console.assert(false);");
    h.eval("console.assert(0, 'x is', 5);");
    let messages = h.messages();
    assert_eq!(messages[0].level, ConsoleLevel::Error);
    assert_eq!(messages[0].message, "Assertion failed");
    assert_eq!(messages[1].message, "Assertion failed: x is 5");
}

#[test]
fn dir_shows_structure_without_a_format_pass() {
    let h = Harness::new();
    // A leading `%s` is data here, not a directive, and only one argument is
    // taken.
    h.eval("console.dir('%s', 'ignored');");
    h.eval("console.dir({a: 1});");
    let messages = h.messages();
    assert_eq!(messages[0].message, r#""%s""#);
    assert_eq!(messages[0].args.len(), 1);
    assert_eq!(messages[1].message, "{a: 1}");
}

#[test]
fn group_depth_rises_and_falls() {
    let h = Harness::new();
    h.eval(
        "console.log('before');
         console.group('outer');
         console.log('inside');
         console.groupCollapsed('inner');
         console.log('deeper');
         console.groupEnd();
         console.log('back');
         console.groupEnd();
         console.log('after');
         console.groupEnd();
         console.log('never negative');",
    );
    let seen: Vec<_> = h
        .messages()
        .iter()
        .map(|m| (m.message.clone(), m.group_depth))
        .collect();
    assert_eq!(
        seen,
        [
            ("before".to_owned(), 0),
            // The label itself is emitted at the *outer* depth.
            ("outer".to_owned(), 0),
            ("inside".to_owned(), 1),
            ("inner".to_owned(), 1),
            ("deeper".to_owned(), 2),
            ("back".to_owned(), 1),
            ("after".to_owned(), 0),
            ("never negative".to_owned(), 0),
        ]
    );
}

#[test]
fn a_console_call_records_its_source_location() {
    let h = Harness::new();
    h.eval("function log(){ console.log('here') }\nlog();");
    let messages = h.messages();
    let at = messages[0].location.as_ref().expect("a location");
    assert_eq!(at.url, "http://x/test.js");
    assert_eq!(at.function.as_deref(), Some("log"));
    assert_eq!(at.line, 1);
    assert!(at.column > 0);
    assert!(messages[0].timestamp > 0.0);
}

#[test]
fn an_engine_message_has_no_arguments_or_location() {
    let h = Harness::with_html("<html><body></body></html>");
    // `document.write` outside a parser script is announced, not silently
    // dropped — and the announcement comes from the engine, not from script.
    h.eval("document.write('<p>late</p>');");
    let messages = h.messages();
    assert_eq!(messages[0].level, ConsoleLevel::Warn);
    assert!(messages[0].args.is_empty());
    assert!(messages[0].location.is_none());
}
