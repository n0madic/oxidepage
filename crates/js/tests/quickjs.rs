//! Contract tests for the QuickJS backend: every behavior the bindings and
//! event loop depend on.

use std::cell::Cell;
use std::rc::Rc;

use oxidepage_js::{
    HostFn, JsEngine, JsRealm, JsThrow, JsValue, PropertyDef, QuickJsEngine, RealmOptions,
    ValueKind,
};

fn realm() -> impl JsRealm {
    QuickJsEngine
        .new_realm(RealmOptions::default())
        .expect("realm creation")
}

#[test]
fn eval_primitives_roundtrip() {
    let realm = realm();
    realm.with_scope(|s| {
        assert!(matches!(s.eval("1 + 2", "t").unwrap(), JsValue::Number(n) if n == 3.0));
        assert!(matches!(s.eval("'a' + 'b'", "t").unwrap(), JsValue::String(v) if v == "ab"));
        assert!(matches!(s.eval("true", "t").unwrap(), JsValue::Bool(true)));
        assert!(matches!(s.eval("null", "t").unwrap(), JsValue::Null));
        assert!(matches!(
            s.eval("undefined", "t").unwrap(),
            JsValue::Undefined
        ));
        assert!(matches!(s.eval("({})", "t").unwrap(), JsValue::Object(_)));
    });
}

#[test]
fn eval_exception_carries_message_and_value() {
    let realm = realm();
    realm.with_scope(|s| {
        let err = s.eval("throw new TypeError('boom')", "t").unwrap_err();
        let oxidepage_js::JsError::Exception {
            name,
            message,
            stack,
            value,
        } = err
        else {
            panic!("expected an exception");
        };
        assert_eq!(name.as_deref(), Some("TypeError"));
        // The message is *bare* now — the stack is data beside it, not glued on.
        assert_eq!(message, "boom");
        assert_eq!(stack.first().map(|f| f.url.as_str()), Some("t"));
        assert!(value.is_some());
        // The realm must stay usable after an exception.
        assert!(matches!(s.eval("2", "t").unwrap(), JsValue::Number(n) if n == 2.0));
    });
}

#[test]
fn host_function_is_callable_and_can_throw() {
    let realm = realm();
    realm.with_scope(|s| {
        let double: HostFn = Rc::new(|_s, call| match call.arg(0) {
            JsValue::Number(n) => Ok(JsValue::Number(n * 2.0)),
            _ => Err(JsThrow::Type("expected a number".into())),
        });
        let func = s.new_function("double", 1, double).unwrap();
        s.set(&s.global(), "double", &JsValue::Object(func)).unwrap();

        assert!(matches!(s.eval("double(21)", "t").unwrap(), JsValue::Number(n) if n == 42.0));
        let caught = s
            .eval(
                "(() => { try { double('x'); return 'no'; } catch (e) { return e instanceof TypeError ? e.message : 'wrong'; } })()",
                "t",
            )
            .unwrap();
        assert!(matches!(caught, JsValue::String(m) if m == "expected a number"));
    });
}

#[test]
fn host_object_prototype_methods_and_payload() {
    let realm = realm();
    realm.with_scope(|s| {
        let proto = s.new_object().unwrap();
        let describe: HostFn = Rc::new(|s, call| {
            let (tag, data) = s
                .host_payload(&call.this)
                .ok_or_else(|| JsThrow::Type("not a host object".into()))?;
            Ok(JsValue::String(format!("{tag}:{data}")))
        });
        let method = s.new_function("describe", 0, describe).unwrap();
        s.define_property(
            &proto,
            "describe",
            PropertyDef::Value {
                value: &JsValue::Object(method),
                writable: true,
                enumerable: false,
                configurable: true,
            },
        )
        .unwrap();

        let host = s.new_host_object(Some(&proto), 7, 99).unwrap();
        assert_eq!(
            s.host_payload(&JsValue::Object(host.clone())),
            Some((7, 99))
        );
        s.set(&s.global(), "host", &JsValue::Object(host)).unwrap();
        let result = s.eval("host.describe()", "t").unwrap();
        assert!(matches!(result, JsValue::String(v) if v == "7:99"));
        // Methods are non-enumerable, WebIDL-style.
        let keys = s
            .eval("Object.keys(Object.getPrototypeOf(host)).length", "t")
            .unwrap();
        assert!(matches!(keys, JsValue::Number(n) if n == 0.0));
    });
}

#[test]
fn constructor_supports_new_and_instanceof() {
    let realm = realm();
    realm.with_scope(|s| {
        let proto = s.new_object().unwrap();
        let construct: HostFn = Rc::new(|s, call| {
            // `this` is the new.target; a plain call passes something else.
            if !s.is_function(&call.this) {
                return Err(JsThrow::Type("Constructor requires 'new'".into()));
            }
            Ok(JsValue::Object(s.new_host_object(None, 1, 5)?))
        });
        let ctor = s.new_constructor("Widget", 0, &proto, construct).unwrap();
        s.set(&s.global(), "Widget", &JsValue::Object(ctor)).unwrap();

        assert!(matches!(
            s.eval("new Widget() instanceof Widget", "t").unwrap(),
            JsValue::Bool(true)
        ));
        assert!(matches!(
            s.eval("Widget.prototype.constructor === Widget", "t").unwrap(),
            JsValue::Bool(true)
        ));
        let plain = s
            .eval(
                "(() => { try { Widget(); return 'no'; } catch (e) { return e instanceof TypeError; } })()",
                "t",
            )
            .unwrap();
        assert!(matches!(plain, JsValue::Bool(true)));
        // Subclassing: prototype follows new.target.
        assert!(matches!(
            s.eval(
                "class W2 extends Widget {}; new W2() instanceof W2",
                "t"
            )
            .unwrap(),
            JsValue::Bool(true)
        ));
    });
}

#[test]
fn values_persist_across_scopes() {
    let realm = realm();
    let func = realm.with_scope(|s| s.eval("(x) => x + 1", "t").unwrap());
    let result = realm.with_scope(|s| {
        s.call(&func, &JsValue::Undefined, &[JsValue::Number(41.0)])
            .unwrap()
    });
    assert!(matches!(result, JsValue::Number(n) if n == 42.0));
}

#[test]
fn nested_call_from_host_callback() {
    // A host function that calls back into JS through the scope it received:
    // the reentrancy pattern event dispatch uses.
    let realm = realm();
    realm.with_scope(|s| {
        let invoke: HostFn = Rc::new(|s, call| {
            let f = call.arg(0);
            let result = s.call(&f, &JsValue::Undefined, &[JsValue::Number(10.0)])?;
            Ok(result)
        });
        let func = s.new_function("invoke", 1, invoke).unwrap();
        s.set(&s.global(), "invoke", &JsValue::Object(func))
            .unwrap();
        let result = s.eval("invoke(x => x * 3)", "t").unwrap();
        assert!(matches!(result, JsValue::Number(n) if n == 30.0));
    });
}

#[test]
fn pump_jobs_runs_promise_reactions() {
    let realm = realm();
    realm.with_scope(|s| {
        s.eval(
            "globalThis.x = 0; Promise.resolve(41).then(v => { globalThis.x = v + 1; })",
            "t",
        )
        .unwrap();
        assert!(matches!(s.eval("x", "t").unwrap(), JsValue::Number(n) if n == 0.0));
    });
    assert!(realm.has_pending_jobs());
    let outcome = realm.pump_jobs();
    assert!(outcome.executed >= 1);
    assert!(outcome.errors.is_empty());
    realm.with_scope(|s| {
        assert!(matches!(s.eval("x", "t").unwrap(), JsValue::Number(n) if n == 42.0));
    });
}

#[test]
fn unhandled_rejections_are_tracked() {
    // A throw inside a reaction produces a rejected promise, not a job
    // error; it surfaces through the rejection tracker.
    let realm = realm();
    let seen: Rc<std::cell::RefCell<Vec<(String, bool)>>> = Rc::default();
    let sink = Rc::clone(&seen);
    realm.set_rejection_tracker(Some(Box::new(move |reason, is_handled| {
        sink.borrow_mut().push((reason.rendered(), is_handled));
    })));
    realm.with_scope(|s| {
        s.eval(
            "Promise.resolve().then(() => { throw new Error('job failed'); })",
            "t",
        )
        .unwrap();
    });
    realm.pump_jobs();
    let seen = seen.borrow();
    assert!(
        seen.iter()
            .any(|(m, handled)| m.contains("job failed") && !handled),
        "expected an unhandled rejection, got {seen:?}"
    );
}

#[test]
fn scope_level_pump_jobs() {
    let realm = realm();
    realm.with_scope(|s| {
        s.eval(
            "globalThis.y = 0; Promise.resolve(7).then(v => { globalThis.y = v; })",
            "t",
        )
        .unwrap();
        let outcome = s.pump_jobs();
        assert!(outcome.executed >= 1);
        assert!(matches!(s.eval("y", "t").unwrap(), JsValue::Number(n) if n == 7.0));
    });
}

#[test]
fn memory_limit_is_enforced() {
    let realm = QuickJsEngine
        .new_realm(RealmOptions {
            memory_limit: Some(2 * 1024 * 1024),
            ..RealmOptions::default()
        })
        .unwrap();
    realm.with_scope(|s| {
        let err = s.eval(
            "const chunks = []; for (;;) { chunks.push(new Array(65536).fill(1)); }",
            "t",
        );
        assert!(err.is_err(), "allocation loop must hit the memory limit");
    });
}

#[test]
fn interrupt_stops_runaway_script() {
    let realm = realm();
    let calls = Rc::new(Cell::new(0u32));
    let calls_in_handler = Rc::clone(&calls);
    realm.set_interrupt(Some(Box::new(move || {
        calls_in_handler.set(calls_in_handler.get() + 1);
        calls_in_handler.get() > 8
    })));
    realm.with_scope(|s| {
        let err = s.eval("for (;;) {}", "t");
        assert!(err.is_err(), "infinite loop must be interrupted");
    });
    assert!(calls.get() > 8);
    realm.set_interrupt(None);
    // The realm stays usable after an interrupt.
    realm.with_scope(|s| {
        assert!(matches!(s.eval("1", "t").unwrap(), JsValue::Number(n) if n == 1.0));
    });
}

#[test]
fn finalized_host_objects_are_reported() {
    let realm = realm();
    realm.with_scope(|s| {
        let host = s.new_host_object(None, 3, 12).unwrap();
        s.set(&s.global(), "keep", &JsValue::Object(host)).unwrap();
        s.set(&s.global(), "keep", &JsValue::Null).unwrap();
    });
    realm.run_gc();
    let finalized = realm.take_finalized();
    assert!(
        finalized.contains(&(3, 12)),
        "expected (3, 12) in {finalized:?}"
    );
    // Drained: a second take reports nothing.
    assert!(realm.take_finalized().is_empty());
}

/// A brand-check miss is a normal outcome — the bindings probe arbitrary
/// script values — and must leave the context clean. The engine's own check
/// throws a TypeError on a miss and reports it as a plain `false`; if that
/// exception stays pending it is picked up by the *next* thing that inspects
/// the context, surfacing as a stack-less "RustClass object expected" against
/// whichever innocent script ran afterwards.
#[test]
fn host_payload_miss_leaves_no_pending_exception() {
    let realm = realm();
    realm.with_scope(|s| {
        let plain = s.eval("({})", "t").unwrap();
        assert_eq!(s.host_payload(&plain), None);
        // A proxy over a plain object: the unwrap loop probes both links.
        let proxied = s.eval("new Proxy({}, {})", "t").unwrap();
        assert_eq!(s.host_payload(&proxied), None);

        s.eval("Promise.resolve().then(() => {})", "t").unwrap();
        let outcome = s.pump_jobs();
        assert_eq!(outcome.executed, 1);
        assert!(
            outcome.errors.is_empty(),
            "brand-check miss poisoned the context: {:?}",
            outcome.errors
        );
    });
}

#[test]
fn strict_equals_preserves_object_identity() {
    let realm = realm();
    realm.with_scope(|s| {
        let a = s.eval("globalThis.obj = {}; obj", "t").unwrap();
        let b = s.eval("obj", "t").unwrap();
        let c = s.eval("({})", "t").unwrap();
        assert!(s.strict_equals(&a, &b));
        assert!(!s.strict_equals(&a, &c));
        assert!(s.strict_equals(&JsValue::Number(1.0), &JsValue::Number(1.0)));
        assert!(!s.strict_equals(&JsValue::Number(1.0), &JsValue::String("1".into())));
    });
}

#[test]
fn arrays_and_accessors() {
    let realm = realm();
    realm.with_scope(|s| {
        let arr = s
            .new_array(&[JsValue::Number(1.0), JsValue::String("two".into())])
            .unwrap();
        assert_eq!(s.array_length(&arr).unwrap(), 2);
        assert!(matches!(s.array_get(&arr, 1).unwrap(), JsValue::String(v) if v == "two"));
        assert!(s.is_array(&JsValue::Object(arr)));

        // Accessor property via define_property.
        let obj = s.new_object().unwrap();
        let getter: HostFn = Rc::new(|_s, _c| Ok(JsValue::Number(5.0)));
        let getter = s.new_function("get x", 0, getter).unwrap();
        s.define_property(
            &obj,
            "x",
            PropertyDef::Accessor {
                getter: Some(&JsValue::Object(getter)),
                setter: None,
                enumerable: true,
                configurable: true,
            },
        )
        .unwrap();
        s.set(&s.global(), "acc", &JsValue::Object(obj)).unwrap();
        assert!(matches!(s.eval("acc.x", "t").unwrap(), JsValue::Number(n) if n == 5.0));
    });
}

#[test]
fn coercions() {
    let realm = realm();
    realm.with_scope(|s| {
        let obj = s.eval("({ toString: () => 'coerced' })", "t").unwrap();
        assert_eq!(s.coerce_string(&obj).unwrap(), "coerced");
        assert_eq!(
            s.coerce_number(&JsValue::String("41".into())).unwrap(),
            41.0
        );
        assert_eq!(s.coerce_string(&JsValue::Number(1.5)).unwrap(), "1.5");
        assert!(
            s.coerce_number(&JsValue::String("nope".into()))
                .unwrap()
                .is_nan()
        );
    });
}

// === ES modules (Phase 3) ===

struct MemModules {
    files: std::collections::HashMap<String, String>,
}

impl oxidepage_js::ModuleSource for MemModules {
    fn resolve(&self, referrer: &str, specifier: &str) -> Result<String, String> {
        // Resolve `./name` relative to the referrer's directory; bare names
        // are returned verbatim.
        if let Some(rel) = specifier.strip_prefix("./") {
            let base = referrer.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
            Ok(format!("{base}/{rel}"))
        } else {
            Ok(specifier.to_owned())
        }
    }
    fn load(&self, url: &str) -> Result<String, String> {
        self.files
            .get(url)
            .cloned()
            .ok_or_else(|| format!("module not found: {url}"))
    }
}

#[test]
fn module_with_static_import_and_import_meta_url() {
    let realm = realm();
    let mut files = std::collections::HashMap::new();
    files.insert(
        "app/dep.js".to_owned(),
        "export const value = 40;".to_owned(),
    );
    realm.set_module_loader(Rc::new(MemModules { files }));

    let entry = r#"
        import { value } from './dep.js';
        globalThis.result = value + 2;
        globalThis.metaUrl = import.meta.url;
    "#;
    realm.with_scope(|s| {
        let promise = s.eval_module(entry, "app/main.js").expect("module eval");
        // Synchronous module (no top-level await) settles immediately.
        s.pump_jobs();
        assert_eq!(
            s.promise_state(&promise),
            Some(oxidepage_js::PromiseState::Fulfilled)
        );
        assert!(matches!(s.eval("globalThis.result", "t").unwrap(), JsValue::Number(n) if n == 42.0));
        assert!(
            matches!(s.eval("globalThis.metaUrl", "t").unwrap(), JsValue::String(u) if u == "app/main.js")
        );
    });
}

#[test]
fn lone_surrogate_string_imports_lossy() {
    let realm = realm();
    realm.with_scope(|s| {
        // An unpaired surrogate imports as U+FFFD in place, not as the empty
        // string (which `unwrap_or_default()` used to yield).
        let v = s
            .eval("'abc' + String.fromCharCode(0xD800) + 'def'", "t")
            .unwrap();
        assert!(
            matches!(v, JsValue::String(ref x) if x == "abc\u{FFFD}def"),
            "got {v:?}"
        );
        // A valid surrogate pair round-trips unchanged.
        let v2 = s
            .eval("'x' + String.fromCharCode(0xD83D, 0xDE00) + 'y'", "t")
            .unwrap();
        assert!(
            matches!(v2, JsValue::String(ref x) if x == "x\u{1F600}y"),
            "got {v2:?}"
        );
    });
}

#[test]
fn exception_splits_into_name_message_and_frames() {
    let realm = realm();
    realm.with_scope(|s| {
        let err = s
            .eval(
                "function inner(){ [1].forEach(function cb(){ throw new TypeError('boom') }) }\n\
                 function outer(){ inner() }\n\
                 outer()",
                "http://x/t.js",
            )
            .unwrap_err();
        assert_eq!(err.name(), Some("TypeError"));
        assert_eq!(err.to_string(), "boom");
        let functions: Vec<_> = err
            .stack()
            .iter()
            .map(|f| f.function.as_deref().unwrap_or(""))
            .collect();
        // Innermost first, and `forEach`'s native frame is gone.
        assert_eq!(functions, ["cb", "inner", "outer", "<eval>"]);
        assert!(err.stack().iter().all(|f| f.url == "http://x/t.js"));
        assert_eq!(err.stack()[0].line, 1);
        assert!(err.stack()[0].column > 0);
        // `rendered()` puts the two halves back together.
        let rendered = err.rendered();
        assert!(
            rendered.starts_with("boom\n    at cb (http://x/t.js:1:"),
            "{rendered}"
        );
    });
}

#[test]
fn a_thrown_non_error_has_no_name_or_stack() {
    let realm = realm();
    realm.with_scope(|s| {
        let err = s.eval("throw 'plain'", "t").unwrap_err();
        assert_eq!(err.name(), None);
        assert_eq!(err.to_string(), "plain");
        assert!(err.stack().is_empty());
    });
}

#[test]
fn tracked_rejections_keep_their_name_and_stack() {
    // The page crate reports these as errors and retracts them by identity, so
    // the tracker must carry the same structure an uncaught throw does.
    let realm = realm();
    let seen: Rc<std::cell::RefCell<Vec<oxidepage_js::JsError>>> = Rc::default();
    let sink = Rc::clone(&seen);
    realm.set_rejection_tracker(Some(Box::new(move |reason, is_handled| {
        if !is_handled {
            sink.borrow_mut().push(reason);
        }
    })));
    realm.with_scope(|s| {
        s.eval(
            "Promise.resolve().then(function reaction(){ throw new RangeError('late') })",
            "http://x/j.js",
        )
        .unwrap();
    });
    realm.pump_jobs();
    let seen = seen.borrow();
    assert_eq!(seen.len(), 1, "got {seen:?}");
    assert_eq!(seen[0].name(), Some("RangeError"));
    assert_eq!(seen[0].to_string(), "late");
    assert_eq!(
        seen[0].stack().first().and_then(|f| f.function.as_deref()),
        Some("reaction")
    );
}

#[test]
fn value_kind_separates_the_object_subtypes() {
    let realm = realm();
    realm.with_scope(|s| {
        let kind = |src: &str| s.value_kind(&s.eval(src, "t").unwrap());
        assert_eq!(kind("undefined"), ValueKind::Undefined);
        assert_eq!(kind("null"), ValueKind::Null);
        assert_eq!(kind("true"), ValueKind::Bool);
        assert_eq!(kind("1.5"), ValueKind::Number);
        assert_eq!(kind("'s'"), ValueKind::String);
        assert_eq!(kind("10n"), ValueKind::BigInt);
        assert_eq!(kind("Symbol('d')"), ValueKind::Symbol);
        assert_eq!(kind("(function f(){})"), ValueKind::Function);
        assert_eq!(kind("(class C {})"), ValueKind::Function);
        assert_eq!(kind("[1]"), ValueKind::Array);
        assert_eq!(kind("new TypeError('x')"), ValueKind::Error);
        assert_eq!(kind("Promise.resolve()"), ValueKind::Promise);
        assert_eq!(kind("({})"), ValueKind::Object);
        assert_eq!(kind("new Date()"), ValueKind::Object);
    });
}

#[test]
fn symbol_description_reads_what_tostring_cannot() {
    let realm = realm();
    realm.with_scope(|s| {
        let sym = s.eval("Symbol('desc')", "t").unwrap();
        // The reason this primitive exists: `ToString` on a symbol throws.
        assert!(s.coerce_string(&sym).is_err());
        assert_eq!(s.symbol_description(&sym).as_deref(), Some("desc"));
        let bare = s.eval("Symbol()", "t").unwrap();
        assert_eq!(s.symbol_description(&bare), None);
        assert_eq!(s.symbol_description(&JsValue::Number(1.0)), None);
    });
}

#[test]
fn own_enumerable_keys_are_object_keys() {
    let realm = realm();
    realm.with_scope(|s| {
        let obj = s
            .eval(
                "const o = { b: 1, a: 2, 2: 'two', 1: 'one' };\n\
                 Object.defineProperty(o, 'hidden', { value: 3, enumerable: false });\n\
                 o[Symbol('s')] = 4;\n\
                 Object.setPrototypeOf(o, { inherited: 5 });\n\
                 o",
                "t",
            )
            .unwrap();
        let JsValue::Object(obj) = obj else {
            panic!("expected an object");
        };
        // Integer-like keys ascending first, then insertion order — and
        // nothing non-enumerable, symbol-keyed or inherited.
        let (keys, total) = s.own_enumerable_keys(&obj, 100).unwrap();
        assert_eq!(keys, ["1", "2", "b", "a"]);
        assert_eq!(total, 4);
        // The limit bounds what is *materialized*, but the total is still
        // reported, so a caller can truncate honestly.
        let (keys, total) = s.own_enumerable_keys(&obj, 2).unwrap();
        assert_eq!(keys, ["1", "2"]);
        assert_eq!(total, 4);
        assert_eq!(s.own_enumerable_keys(&obj, 0).unwrap(), (Vec::new(), 4));
    });
}

#[test]
fn capture_stack_sees_the_caller_from_a_host_callback() {
    let realm = realm();
    let seen: Rc<std::cell::RefCell<Vec<String>>> = Rc::default();
    let sink = Rc::clone(&seen);
    realm.with_scope(|s| {
        // Nothing on the JS stack yet.
        assert!(s.capture_stack().is_empty());
        let probe: HostFn = Rc::new(move |scope, _call| {
            sink.borrow_mut().extend(
                scope
                    .capture_stack()
                    .into_iter()
                    .map(|f| format!("{}@{}:{}", f.function.unwrap_or_default(), f.url, f.line)),
            );
            Ok(JsValue::Undefined)
        });
        let f = s.new_function("probe", 0, probe).unwrap();
        let global = s.global();
        s.set(&global, "probe", &JsValue::Object(f)).unwrap();
        s.eval("function caller(){ probe() }\ncaller()", "http://x/p.js")
            .unwrap();
    });
    let seen = seen.borrow();
    assert_eq!(
        seen.as_slice(),
        ["caller@http://x/p.js:1", "<eval>@http://x/p.js:2"],
        "capture_stack must drop the native frame and keep the JS callers"
    );
}

#[test]
fn capture_location_is_the_innermost_frame() {
    let realm = realm();
    let seen: Rc<std::cell::RefCell<Vec<Option<oxidepage_js::StackFrame>>>> = Rc::default();
    let sink = Rc::clone(&seen);
    realm.with_scope(|s| {
        assert!(s.capture_location().is_none());
        let probe: HostFn = Rc::new(move |scope, _call| {
            sink.borrow_mut().push(scope.capture_location());
            Ok(JsValue::Undefined)
        });
        let f = s.new_function("probe", 0, probe).unwrap();
        let global = s.global();
        s.set(&global, "probe", &JsValue::Object(f)).unwrap();
        s.eval("function caller(){ probe() }\ncaller()", "http://x/p.js")
            .unwrap();
    });
    let seen = seen.borrow();
    let frame = seen[0].as_ref().expect("a frame");
    assert_eq!(frame.function.as_deref(), Some("caller"));
    assert_eq!(frame.url, "http://x/p.js");
}

#[test]
fn a_thrown_symbol_is_named_and_leaves_no_pending_exception() {
    let realm = realm();
    realm.with_scope(|s| {
        // `ToString` on a symbol throws; naming it must not leave *that*
        // exception pending for unrelated script to be blamed for.
        let err = s.eval("throw Symbol('boom')", "t").unwrap_err();
        assert_eq!(err.to_string(), "Symbol(boom)");
        assert!(matches!(s.eval("1 + 1", "t").unwrap(), JsValue::Number(n) if n == 2.0));
        let err = s.eval("throw Symbol()", "t").unwrap_err();
        assert_eq!(err.to_string(), "Symbol()");
    });
}

#[test]
fn capture_stack_leaves_no_pending_exception() {
    let realm = realm();
    realm.with_scope(|s| {
        s.eval("globalThis.x = 1", "t").unwrap();
        let _ = s.capture_stack();
        // A stack capture that left a pending exception would surface here,
        // blamed on unrelated script.
        assert!(matches!(s.eval("x + 1", "t").unwrap(), JsValue::Number(n) if n == 2.0));
    });
}
