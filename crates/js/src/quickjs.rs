//! QuickJS-NG backend for the [`JsEngine`] traits, via `rquickjs`.
//!
//! Lifetime rules this module upholds:
//!
//! - `rquickjs::Context::with` must not be entered re-entrantly (the runtime
//!   lock is a `RefCell`). Host callbacks therefore never touch the realm
//!   handle; they operate through the [`QuickScope`] built from the `Ctx`
//!   the engine passed them.
//! - Every [`Persistent`] engine reference must be dropped before the
//!   [`Runtime`]. `RealmInner`'s field order guarantees that for the helper
//!   functions and the host state; embedders uphold it for their own state
//!   by dropping it before the realm (see `JsObject` docs).
//! - Host functions capture only a `Weak` realm reference: JS objects
//!   holding strong references back to the realm would keep the runtime
//!   alive forever.

use std::any::Any;
use std::cell::RefCell;
use std::rc::{Rc, Weak};

use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::context::EvalOptions;
use rquickjs::function::{Constructor, Rest, This};
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::object::Filter;
use rquickjs::prelude::Coerced;
use rquickjs::{
    Array, ArrayBuffer, Atom, Context, Ctx, Exception, Function, JsLifetime, Module, Object,
    Persistent, Runtime, Type, Value, qjs,
};

use crate::error::{JsError, JsThrow, StackFrame, parse_stack};
use crate::value::{JsObject, JsValue};
use crate::{
    HostCall, HostFn, JobsOutcome, JsEngine, JsRealm, JsScope, ModuleSource, PromiseState,
    PropertyDef, RealmOptions, ValueKind,
};

/// The QuickJS-NG engine backend.
#[derive(Default)]
pub struct QuickJsEngine;

impl JsEngine for QuickJsEngine {
    type Realm = QuickJsRealm;

    fn new_realm(&self, opts: RealmOptions) -> Result<Self::Realm, JsError> {
        QuickJsRealm::new(opts)
    }
}

/// Queue of `(tag, data)` payloads of garbage-collected host objects.
type FinalizedQueue = Rc<RefCell<Vec<(u32, u64)>>>;

/// Cached JS helper functions the scope implementation leans on for
/// operations rquickjs has no direct safe API for.
struct Helpers {
    define_property: Persistent<Function<'static>>,
    strict_equals: Persistent<Function<'static>>,
}

struct RealmInner {
    // Drop order matters: `state` and `helpers` hold `Persistent` references
    // and must drop before `context`/`rt`; `fin` must outlive `rt` because
    // the runtime's final GC pushes into it.
    state: RefCell<Option<Rc<dyn Any>>>,
    helpers: RefCell<Option<Helpers>>,
    context: Context,
    rt: Runtime,
    fin: FinalizedQueue,
}

/// A QuickJS realm.
pub struct QuickJsRealm {
    inner: Rc<RealmInner>,
}

impl QuickJsRealm {
    fn new(opts: RealmOptions) -> Result<Self, JsError> {
        let rt = Runtime::new().map_err(|e| JsError::Engine(e.to_string()))?;
        if let Some(limit) = opts.memory_limit {
            rt.set_memory_limit(limit);
        }
        if let Some(size) = opts.max_stack_size {
            rt.set_max_stack_size(size);
        }
        if let Some(threshold) = opts.gc_threshold {
            rt.set_gc_threshold(threshold);
        }
        let context = Context::full(&rt).map_err(|e| JsError::Engine(e.to_string()))?;
        Ok(Self {
            inner: Rc::new(RealmInner {
                state: RefCell::new(None),
                helpers: RefCell::new(None),
                context,
                rt,
                fin: Rc::new(RefCell::new(Vec::new())),
            }),
        })
    }
}

impl QuickJsRealm {
    /// Re-anchors this realm's native-stack budget at the current stack depth.
    ///
    /// QuickJS records a runtime's stack ceiling **once, in `JS_NewRuntime`**
    /// (`quickjs.c:2019`), and rquickjs's own `update_stack_top` compiles to
    /// nothing without the `parallel` feature (`runtime/raw.rs:194`) — so
    /// `Context::with`'s call to it is a no-op for us. A realm therefore
    /// measures `max_stack_size` from wherever it happened to be *created*,
    /// giving a realm created deep in the page thread's stack an effective
    /// budget of `max_stack_size + (creation_depth - entry_depth)`: unbounded,
    /// and anchored to the wrong frame. With one runtime per world a world is
    /// routinely created deep (inside an embedder job, or from another world's
    /// host callback), so this is load-bearing rather than theoretical —
    /// measured at 1.53x the intended budget for a realm created 512 KiB down.
    ///
    /// Re-anchoring on entry makes each world's budget exactly
    /// `max_stack_size` from its own entry point, which is what lets N nested
    /// worlds be bounded against one thread stack. Nothing needs restoring on
    /// exit: entering a runtime already on the stack is refused a layer up, so
    /// every entry is the outermost one for its own runtime.
    #[allow(unsafe_code)]
    fn anchor_stack(&self) {
        // SAFETY: `get_runtime_ptr` returns this realm's live runtime, which
        // `RealmInner` owns and keeps alive for the call.
        unsafe { qjs::JS_UpdateStackTop(self.inner.context.get_runtime_ptr()) }
    }
}

impl JsRealm for QuickJsRealm {
    fn with_scope<T>(&self, f: impl FnOnce(&dyn JsScope) -> T) -> T {
        self.anchor_stack();
        self.inner.context.with(|ctx| {
            let scope = QuickScope {
                ctx,
                inner: Rc::clone(&self.inner),
            };
            f(&scope)
        })
    }

    fn set_state(&self, state: Rc<dyn Any>) {
        *self.inner.state.borrow_mut() = Some(state);
    }

    fn state(&self) -> Option<Rc<dyn Any>> {
        self.inner.state.borrow().clone()
    }

    fn set_module_loader(&self, source: Rc<dyn ModuleSource>) {
        self.inner
            .rt
            .set_loader(SourceResolver(Rc::clone(&source)), SourceLoader(source));
    }

    fn pump_jobs(&self) -> JobsOutcome {
        self.anchor_stack();
        let mut out = JobsOutcome::default();
        loop {
            match self.inner.rt.execute_pending_job() {
                Ok(true) => out.executed += 1,
                Ok(false) => break,
                Err(job_error) => {
                    out.executed += 1;
                    let error = job_error.0.with(|ctx| {
                        let scope = QuickScope {
                            ctx,
                            inner: Rc::clone(&self.inner),
                        };
                        let caught = scope.ctx.catch();
                        scope.exception_from(caught)
                    });
                    out.errors.push(error);
                }
            }
        }
        out
    }

    fn has_pending_jobs(&self) -> bool {
        self.inner.rt.is_job_pending()
    }

    fn run_gc(&self) {
        self.inner.rt.run_gc();
    }

    fn take_finalized(&self) -> Vec<(u32, u64)> {
        std::mem::take(&mut *self.inner.fin.borrow_mut())
    }

    fn set_interrupt(&self, callback: Option<Box<dyn FnMut() -> bool>>) {
        self.inner.rt.set_interrupt_handler(callback);
    }

    fn set_rejection_tracker(&self, callback: Option<Box<dyn Fn(JsError, bool)>>) {
        match callback {
            Some(callback) => self
                .inner
                .rt
                .set_host_promise_rejection_tracker(Some(Box::new(
                    move |_ctx, _promise, reason, is_handled| {
                        // No `value`: the tracker must not capture the realm
                        // (the runtime owns this closure, so a strong
                        // reference back would be a cycle), and importing the
                        // reason needs a scope built from it.
                        let (name, message, stack) = split_exception(&reason);
                        callback(
                            JsError::Exception {
                                name,
                                message,
                                stack,
                                value: None,
                            },
                            is_handled,
                        );
                    },
                ))),
            None => self.inner.rt.set_host_promise_rejection_tracker(None),
        }
    }

    fn set_memory_limit(&self, bytes: usize) {
        self.inner.rt.set_memory_limit(bytes);
    }

    fn memory_used(&self) -> i64 {
        self.inner.rt.memory_usage().memory_used_size
    }
}

/// The per-instance payload of every DOM/host object exposed to JS.
///
/// One native class serves all host interfaces; per-interface behavior
/// (prototype chains, brand checks) is layered on top by the bindings via
/// the `(tag, data)` payload.
struct HostObject {
    tag: u32,
    data: u64,
    fin: FinalizedQueue,
}

impl Drop for HostObject {
    fn drop(&mut self) {
        self.fin.borrow_mut().push((self.tag, self.data));
    }
}

impl<'js> Trace<'js> for HostObject {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

// SAFETY: `HostObject` owns no lifetime-bound engine references, so changing
// the realm lifetime parameter is a no-op.
#[allow(unsafe_code)]
unsafe impl<'js> JsLifetime<'js> for HostObject {
    type Changed<'to> = HostObject;
}

impl<'js> JsClass<'js> for HostObject {
    const NAME: &'static str = "HostObject";
    type Mutable = Readable;

    fn prototype(_ctx: &Ctx<'js>) -> rquickjs::Result<Option<Object<'js>>> {
        Ok(None)
    }

    fn constructor(_ctx: &Ctx<'js>) -> rquickjs::Result<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// An entered QuickJS realm.
pub struct QuickScope<'js> {
    ctx: Ctx<'js>,
    inner: Rc<RealmInner>,
}

/// Converts a JS string to Rust, replacing every unpaired surrogate with
/// U+FFFD (the WHATWG "convert to a scalar value string" operation). QuickJS
/// keeps lone surrogates internally, so a direct UTF-8 conversion fails on
/// them — the previous `unwrap_or_default()` then dropped the *entire* string
/// rather than just the offending code unit.
#[allow(unsafe_code)]
fn string_to_lossy(s: &rquickjs::String<'_>) -> String {
    // Fast path: strings without lone surrogates convert directly.
    if let Ok(text) = s.to_string() {
        return text;
    }
    // Slow path: read the raw UTF-16 code units and decode them, mapping each
    // unpaired surrogate to U+FFFD.
    unsafe {
        let ctx = s.ctx().as_raw().as_ptr();
        let mut len: qjs::size_t = 0;
        let ptr = qjs::JS_ToCStringLenUTF16(ctx, &mut len, s.as_value().as_raw());
        if ptr.is_null() {
            return String::new();
        }
        let units = std::slice::from_raw_parts(ptr, len as usize);
        let out = char::decode_utf16(units.iter().copied())
            .map(|r| r.unwrap_or('\u{FFFD}'))
            .collect();
        qjs::JS_FreeCStringUTF16(ctx, ptr);
        out
    }
}

impl<'js> QuickScope<'js> {
    /// Engine value → neutral value. Primitives convert eagerly; everything
    /// else becomes a persistent reference.
    fn import(&self, value: Value<'js>) -> JsValue {
        if value.is_undefined() {
            JsValue::Undefined
        } else if value.is_null() {
            JsValue::Null
        } else if let Some(b) = value.as_bool() {
            JsValue::Bool(b)
        } else if let Some(n) = value.as_number() {
            JsValue::Number(n)
        } else if let Some(s) = value.as_string() {
            JsValue::String(string_to_lossy(s))
        } else {
            self.import_ref(value)
        }
    }

    fn import_ref(&self, value: Value<'js>) -> JsValue {
        JsValue::Object(JsObject::new(Rc::new(Persistent::save(&self.ctx, value))))
    }

    fn import_obj(&self, object: Object<'js>) -> JsObject {
        JsObject::new(Rc::new(Persistent::save(&self.ctx, object.into_value())))
    }

    /// Neutral value → engine value.
    fn export(&self, value: &JsValue) -> rquickjs::Result<Value<'js>> {
        Ok(match value {
            JsValue::Undefined => Value::new_undefined(self.ctx.clone()),
            JsValue::Null => Value::new_null(self.ctx.clone()),
            JsValue::Bool(b) => Value::new_bool(self.ctx.clone(), *b),
            JsValue::Number(n) => Value::new_number(self.ctx.clone(), *n),
            JsValue::String(s) => rquickjs::String::from_str(self.ctx.clone(), s)?.into_value(),
            JsValue::Object(o) => self.export_obj(o)?,
        })
    }

    fn export_obj(&self, object: &JsObject) -> rquickjs::Result<Value<'js>> {
        let persistent = object
            .0
            .downcast_ref::<Persistent<Value<'static>>>()
            .ok_or(rquickjs::Error::UnrelatedRuntime)?;
        persistent.clone().restore(&self.ctx)
    }

    /// Like [`Self::export_obj`] but requiring an actual object.
    fn export_object(&self, object: &JsObject) -> Result<Object<'js>, JsError> {
        self.export_obj(object)
            .map_err(|e| self.error_from(e))?
            .into_object()
            .ok_or_else(|| JsError::Engine("expected an object reference".into()))
    }

    /// Brand-checks `object` against our host class, returning its payload.
    ///
    /// A miss is an ordinary answer here — the bindings probe values that come
    /// straight from script — but rquickjs's check bottoms out in
    /// `JS_GetOpaque2`, which *throws* a `TypeError` into the context on a
    /// mismatch and then reports the miss as a plain `false`. Left pending,
    /// that exception is picked up by whatever inspects the context next (a
    /// promise job in [`Self::pump_jobs`], say) and surfaces as a stack-less
    /// "RustClass object expected" blamed on unrelated script. So the check
    /// must swallow what it itself raised.
    fn as_host_object(&self, object: &Object<'js>) -> Option<(u32, u64)> {
        // Anything already pending is not ours to drop, and QuickJS would let
        // the check's throw overwrite it, so park it across the check.
        let parked = self.ctx.has_exception().then(|| self.ctx.catch());
        let payload = rquickjs::Class::<HostObject>::from_object(object).map(|class| {
            let payload = class.borrow();
            (payload.tag, payload.data)
        });
        if payload.is_none() && self.ctx.has_exception() {
            let _ = self.ctx.catch();
        }
        if let Some(exception) = parked {
            let _ = self.ctx.throw(exception);
        }
        payload
    }

    /// Converts an rquickjs error into a [`JsError`], catching the pending
    /// exception when there is one.
    fn error_from(&self, err: rquickjs::Error) -> JsError {
        if err.is_exception() || self.ctx.has_exception() {
            let caught = self.ctx.catch();
            self.exception_from(caught)
        } else if matches!(err, rquickjs::Error::UnrelatedRuntime) {
            // One runtime per world (ADR-0033), so this is always the same
            // mistake: a value minted in one world reached a scope entered on
            // another. rquickjs renders it "Restoring Persistent in an
            // unrelated runtime", which sends the reader looking for a GC bug.
            JsError::Engine("value belongs to a different JavaScript world".into())
        } else {
            JsError::Engine(err.to_string())
        }
    }

    /// Structures an already-caught exception value, keeping the value itself.
    fn exception_from(&self, caught: Value<'js>) -> JsError {
        let (name, message, stack) = split_exception(&caught);
        JsError::Exception {
            name,
            message,
            stack,
            value: Some(self.import(caught)),
        }
    }

    /// Raises a host throw as a pending engine exception.
    fn throw_of(&self, throw: JsThrow) -> rquickjs::Error {
        match throw {
            JsThrow::Type(m) => Exception::throw_type(&self.ctx, &m),
            JsThrow::Range(m) => Exception::throw_range(&self.ctx, &m),
            JsThrow::Value(v) => match self.export(&v) {
                Ok(value) => self.ctx.throw(value),
                Err(e) => e,
            },
        }
    }

    /// Builds the host-function trampoline shared by functions and
    /// constructors.
    fn make_function(&self, f: HostFn) -> Result<Function<'js>, JsError> {
        let weak: Weak<RealmInner> = Rc::downgrade(&self.inner);
        Function::new(
            self.ctx.clone(),
            move |ctx: Ctx<'js>,
                  this: This<Value<'js>>,
                  args: Rest<Value<'js>>|
                  -> rquickjs::Result<Value<'js>> {
                let Some(inner) = weak.upgrade() else {
                    return Ok(Value::new_undefined(ctx));
                };
                let scope = QuickScope { ctx, inner };
                let call = HostCall {
                    this: scope.import(this.0),
                    args: args.0.into_iter().map(|v| scope.import(v)).collect(),
                };
                match f(&scope, call) {
                    Ok(value) => scope.export(&value),
                    Err(throw) => Err(scope.throw_of(throw)),
                }
            },
        )
        .map_err(|e| self.error_from(e))
    }

    /// Builds a constructor function whose `prototype` property is `proto`.
    /// `back_ref` wires `proto.constructor` back at it, which every interface
    /// object does and a `[LegacyFactoryFunction]` must not (the prototype
    /// belongs to the interface, and there can be more than one factory).
    fn make_ctor(
        &self,
        name: &str,
        length: u32,
        proto: &JsObject,
        f: HostFn,
        back_ref: bool,
    ) -> Result<JsObject, JsError> {
        // Fix up the returned object's prototype from `new.target.prototype`
        // (subclassing per spec). No captured fallback: the interface
        // prototype must not be captured in the closure (a native reference
        // the GC cannot trace would leak the `proto ↔ constructor` cycle),
        // and `f` already creates its result with the interface prototype.
        let f: HostFn = Rc::new(move |scope, call| {
            let new_target = call.this.clone();
            let result = f(scope, call)?;
            if let (JsValue::Object(result_obj), JsValue::Object(nt)) = (&result, &new_target)
                && scope.is_function(&new_target)
                && let Ok(JsValue::Object(p)) = scope.get(nt, "prototype")
            {
                scope
                    .set_prototype(result_obj, Some(&p))
                    .map_err(JsThrow::from)?;
            }
            Ok(result)
        });
        let proto = self.export_object(proto)?;
        let func = self.make_function(f)?;
        func.set_name(name).map_err(|e| self.error_from(e))?;
        func.set_length(length as usize)
            .map_err(|e| self.error_from(e))?;
        func.set_constructor(true);
        // Wire `ctor.prototype` and `proto.constructor` per WebIDL
        // (non-enumerable, which plain `set` would get wrong).
        self.define_property(
            &self.import_obj(func.clone().into_inner()),
            "prototype",
            PropertyDef::Value {
                value: &self.import_ref(proto.clone().into_value()),
                writable: false,
                enumerable: false,
                configurable: false,
            },
        )?;
        if back_ref {
            let ctor_value = func.clone().into_inner().into_value();
            self.define_property(
                &self.import_obj(proto),
                "constructor",
                PropertyDef::Value {
                    value: &self.import(ctor_value),
                    writable: true,
                    enumerable: false,
                    configurable: true,
                },
            )?;
        }
        Ok(self.import_obj(func.into_inner()))
    }

    /// The engine's backtrace text for the current stack.
    ///
    /// `JS_NewError` builds a backtrace unconditionally, so minting a
    /// throw-away `Error` is the way to read the stack without throwing.
    /// Nothing is left pending: `from_message` constructs an object, it does
    /// not raise it.
    fn backtrace(&self) -> Option<String> {
        Exception::from_message(self.ctx.clone(), "")
            .ok()
            .and_then(|exception| exception.stack())
    }

    fn helpers<T>(&self, f: impl FnOnce(&Helpers) -> T) -> Result<T, JsError> {
        // Evaluate outside any borrow of `helpers`: `ctx.eval` runs JS, and
        // holding the `RefCell` across it risks a re-entrant borrow panic.
        if self.inner.helpers.borrow().is_none() {
            let define_property: Function<'js> = self
                .ctx
                .eval("Object.defineProperty")
                .map_err(|e| self.error_from(e))?;
            let strict_equals: Function<'js> = self
                .ctx
                .eval("(a, b) => a === b")
                .map_err(|e| self.error_from(e))?;
            let helpers = Helpers {
                define_property: Persistent::save(&self.ctx, define_property),
                strict_equals: Persistent::save(&self.ctx, strict_equals),
            };
            let mut slot = self.inner.helpers.borrow_mut();
            if slot.is_none() {
                *slot = Some(helpers);
            }
        }
        let slot = self.inner.helpers.borrow();
        Ok(f(slot.as_ref().expect("helpers just initialized")))
    }
}

impl<'js> JsScope for QuickScope<'js> {
    fn state(&self) -> Option<Rc<dyn Any>> {
        self.inner.state.borrow().clone()
    }

    fn eval(&self, source: &str, filename: &str) -> Result<JsValue, JsError> {
        let mut opts = EvalOptions::default();
        opts.global = true;
        opts.strict = false;
        opts.promise = false;
        opts.filename = Some(filename.to_owned());
        match self.ctx.eval_with_options::<Value<'js>, _>(source, opts) {
            Ok(v) => Ok(self.import(v)),
            Err(e) => Err(self.error_from(e)),
        }
    }

    fn global(&self) -> JsObject {
        self.import_obj(self.ctx.globals())
    }

    fn new_object(&self) -> Result<JsObject, JsError> {
        Object::new(self.ctx.clone())
            .map(|o| self.import_obj(o))
            .map_err(|e| self.error_from(e))
    }

    fn new_object_with_proto(&self, proto: Option<&JsObject>) -> Result<JsObject, JsError> {
        let proto = match proto {
            Some(p) => Some(self.export_object(p)?),
            None => None,
        };
        Object::new_proto(self.ctx.clone(), proto.as_ref())
            .map(|o| self.import_obj(o))
            .map_err(|e| self.error_from(e))
    }

    fn new_array(&self, items: &[JsValue]) -> Result<JsObject, JsError> {
        let array = Array::new(self.ctx.clone()).map_err(|e| self.error_from(e))?;
        for (i, item) in items.iter().enumerate() {
            let value = self.export(item).map_err(|e| self.error_from(e))?;
            array.set(i, value).map_err(|e| self.error_from(e))?;
        }
        Ok(self.import_obj(array.into_object()))
    }

    fn new_array_buffer(&self, bytes: &[u8]) -> Result<JsObject, JsError> {
        // `new_copy` hands the slice to `JS_NewArrayBufferCopy`, so the engine
        // owns the result and nothing here has to outlive the call.
        ArrayBuffer::new_copy(self.ctx.clone(), bytes)
            .map(|buffer| self.import_obj(buffer.into_object()))
            .map_err(|e| self.error_from(e))
    }

    fn new_function(&self, name: &str, length: u32, f: HostFn) -> Result<JsObject, JsError> {
        let func = self.make_function(f)?;
        func.set_name(name).map_err(|e| self.error_from(e))?;
        func.set_length(length as usize)
            .map_err(|e| self.error_from(e))?;
        Ok(self.import_obj(func.into_inner()))
    }

    fn new_constructor(
        &self,
        name: &str,
        length: u32,
        proto: &JsObject,
        f: HostFn,
    ) -> Result<JsObject, JsError> {
        self.make_ctor(name, length, proto, f, true)
    }

    fn new_legacy_factory(
        &self,
        name: &str,
        length: u32,
        proto: &JsObject,
        f: HostFn,
    ) -> Result<JsObject, JsError> {
        self.make_ctor(name, length, proto, f, false)
    }

    fn new_host_object(
        &self,
        proto: Option<&JsObject>,
        tag: u32,
        data: u64,
    ) -> Result<JsObject, JsError> {
        let payload = HostObject {
            tag,
            data,
            fin: Rc::clone(&self.inner.fin),
        };
        let instance = match proto {
            Some(p) => {
                let proto = self.export_object(p)?;
                rquickjs::Class::instance_proto(payload, proto)
            }
            None => rquickjs::Class::instance(self.ctx.clone(), payload),
        }
        .map_err(|e| self.error_from(e))?;
        Ok(self.import_obj(instance.into_inner()))
    }

    fn host_payload(&self, value: &JsValue) -> Option<(u32, u64)> {
        let JsValue::Object(o) = value else {
            return None;
        };
        let value = self.export_obj(o).ok()?;
        let mut object = value.into_object()?;
        // Unwrap proxy chains: the bindings wrap indexed collections in a
        // Proxy, and brand checks must see through it.
        loop {
            if let Some(payload) = self.as_host_object(&object) {
                return Some(payload);
            }
            let proxy = object.as_value().as_proxy()?.clone();
            object = proxy.target().ok()?;
        }
    }

    fn get(&self, obj: &JsObject, key: &str) -> Result<JsValue, JsError> {
        let object = self.export_object(obj)?;
        object
            .get::<_, Value<'js>>(key)
            .map(|v| self.import(v))
            .map_err(|e| self.error_from(e))
    }

    fn set(&self, obj: &JsObject, key: &str, value: &JsValue) -> Result<(), JsError> {
        let object = self.export_object(obj)?;
        let value = self.export(value).map_err(|e| self.error_from(e))?;
        object.set(key, value).map_err(|e| self.error_from(e))
    }

    fn define_property(
        &self,
        obj: &JsObject,
        key: &str,
        prop: PropertyDef<'_>,
    ) -> Result<(), JsError> {
        let descriptor = Object::new(self.ctx.clone()).map_err(|e| self.error_from(e))?;
        let fill = |k: &str, v: Value<'js>| descriptor.set(k, v);
        match prop {
            PropertyDef::Value {
                value,
                writable,
                enumerable,
                configurable,
            } => {
                fill("value", self.export(value).map_err(|e| self.error_from(e))?)
                    .and_then(|()| descriptor.set("writable", writable))
                    .and_then(|()| descriptor.set("enumerable", enumerable))
                    .and_then(|()| descriptor.set("configurable", configurable))
                    .map_err(|e| self.error_from(e))?;
            }
            PropertyDef::Accessor {
                getter,
                setter,
                enumerable,
                configurable,
            } => {
                if let Some(g) = getter {
                    fill("get", self.export(g).map_err(|e| self.error_from(e))?)
                        .map_err(|e| self.error_from(e))?;
                }
                if let Some(s) = setter {
                    fill("set", self.export(s).map_err(|e| self.error_from(e))?)
                        .map_err(|e| self.error_from(e))?;
                }
                descriptor
                    .set("enumerable", enumerable)
                    .and_then(|()| descriptor.set("configurable", configurable))
                    .map_err(|e| self.error_from(e))?;
            }
        }
        let target = self.export_object(obj)?;
        let key = rquickjs::String::from_str(self.ctx.clone(), key)
            .map_err(|e| self.error_from(e))?
            .into_value();
        let define = self.helpers(|h| h.define_property.clone())?;
        let define = define.restore(&self.ctx).map_err(|e| self.error_from(e))?;
        define
            .call::<_, Value<'js>>((target, key, descriptor))
            .map(|_| ())
            .map_err(|e| self.error_from(e))
    }

    fn set_prototype(&self, obj: &JsObject, proto: Option<&JsObject>) -> Result<(), JsError> {
        let object = self.export_object(obj)?;
        let proto = match proto {
            Some(p) => Some(self.export_object(p)?),
            None => None,
        };
        object
            .set_prototype(proto.as_ref())
            .map_err(|e| self.error_from(e))
    }

    fn call(
        &self,
        function: &JsValue,
        this: &JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JsError> {
        let JsValue::Object(o) = function else {
            return Err(JsError::Engine("call target is not a function".into()));
        };
        let value = self.export_obj(o).map_err(|e| self.error_from(e))?;
        let Some(func) = value.as_function() else {
            return Err(JsError::Engine("call target is not a function".into()));
        };
        let mut call_args = rquickjs::function::Args::new(self.ctx.clone(), args.len());
        call_args
            .this(self.export(this).map_err(|e| self.error_from(e))?)
            .map_err(|e| self.error_from(e))?;
        for arg in args {
            call_args
                .push_arg(self.export(arg).map_err(|e| self.error_from(e))?)
                .map_err(|e| self.error_from(e))?;
        }
        func.call_arg::<Value<'js>>(call_args)
            .map(|v| self.import(v))
            .map_err(|e| self.error_from(e))
    }

    fn is_function(&self, value: &JsValue) -> bool {
        match value {
            JsValue::Object(o) => self.export_obj(o).is_ok_and(|v| v.is_function()),
            _ => false,
        }
    }

    fn is_array(&self, value: &JsValue) -> bool {
        match value {
            JsValue::Object(o) => self.export_obj(o).is_ok_and(|v| v.is_array()),
            _ => false,
        }
    }

    fn value_kind(&self, value: &JsValue) -> ValueKind {
        let object = match value {
            JsValue::Undefined => return ValueKind::Undefined,
            JsValue::Null => return ValueKind::Null,
            JsValue::Bool(_) => return ValueKind::Bool,
            JsValue::Number(_) => return ValueKind::Number,
            JsValue::String(_) => return ValueKind::String,
            JsValue::Object(o) => o,
        };
        let Ok(value) = self.export_obj(object) else {
            return ValueKind::Object;
        };
        match value.type_of() {
            Type::Uninitialized | Type::Undefined => ValueKind::Undefined,
            Type::Null => ValueKind::Null,
            Type::Bool => ValueKind::Bool,
            Type::Int | Type::Float => ValueKind::Number,
            Type::BigInt => ValueKind::BigInt,
            Type::String => ValueKind::String,
            Type::Symbol => ValueKind::Symbol,
            // A class is a function too, and reads better as one.
            Type::Function | Type::Constructor => ValueKind::Function,
            Type::Array => ValueKind::Array,
            Type::Exception => ValueKind::Error,
            Type::Promise => ValueKind::Promise,
            Type::Object | Type::Proxy | Type::Module | Type::Unknown => ValueKind::Object,
        }
    }

    fn own_enumerable_keys(
        &self,
        obj: &JsObject,
        limit: usize,
    ) -> Result<(Vec<String>, usize), JsError> {
        let object = self.export_object(obj)?;
        // `Filter::default()` is string-keyed + enumerable-only, i.e. exactly
        // `Object.keys`.
        //
        // Only the first `limit` atoms are turned into Rust strings. The rest
        // are still counted — the caller needs the true total to report an
        // honest truncation — but an object with five million keys must not
        // cost five million `String` allocations for a preview that keeps a
        // hundred of them.
        let mut keys = Vec::new();
        let mut total = 0usize;
        for key in object.own_keys::<Atom<'js>>(Filter::default()) {
            let atom = key.map_err(|e| self.error_from(e))?;
            total += 1;
            if keys.len() < limit {
                match atom.to_js_string() {
                    Ok(s) => keys.push(string_to_lossy(&s)),
                    Err(e) => return Err(self.error_from(e)),
                }
            }
        }
        Ok((keys, total))
    }

    fn symbol_description(&self, value: &JsValue) -> Option<String> {
        let JsValue::Object(o) = value else {
            return None;
        };
        let value = self.export_obj(o).ok()?;
        let symbol = value.as_symbol()?;
        // `Symbol()` has an undefined description; the empty string is a
        // *different*, real description, so only `undefined` maps to `None`.
        match symbol.description().ok()? {
            d if d.is_undefined() => None,
            d => d.as_string().map(string_to_lossy),
        }
    }

    fn capture_stack(&self) -> Vec<StackFrame> {
        self.backtrace().map_or_else(Vec::new, |s| parse_stack(&s))
    }

    fn capture_location(&self) -> Option<StackFrame> {
        // Separate from `capture_stack` because every console call takes this
        // path and keeps exactly one frame: parsing the whole backtrace to
        // drop all but the first is pure allocation.
        self.backtrace()
            .as_deref()
            .and_then(crate::error::parse_first_frame)
    }

    fn strict_equals(&self, a: &JsValue, b: &JsValue) -> bool {
        match (a, b) {
            (JsValue::Object(a), JsValue::Object(b)) => {
                let Ok(equals) = self.helpers(|h| h.strict_equals.clone()) else {
                    return false;
                };
                let Ok(equals) = equals.restore(&self.ctx) else {
                    return false;
                };
                let (Ok(a), Ok(b)) = (self.export_obj(a), self.export_obj(b)) else {
                    return false;
                };
                equals.call::<_, bool>((a, b)).unwrap_or(false)
            }
            (JsValue::Undefined, JsValue::Undefined) | (JsValue::Null, JsValue::Null) => true,
            (JsValue::Bool(a), JsValue::Bool(b)) => a == b,
            (JsValue::Number(a), JsValue::Number(b)) => a == b,
            (JsValue::String(a), JsValue::String(b)) => a == b,
            _ => false,
        }
    }

    fn coerce_string(&self, value: &JsValue) -> Result<String, JsError> {
        if let JsValue::String(s) = value {
            return Ok(s.clone());
        }
        let value = self.export(value).map_err(|e| self.error_from(e))?;
        value
            .get::<Coerced<String>>()
            .map(|c| c.0)
            .map_err(|e| self.error_from(e))
    }

    fn coerce_number(&self, value: &JsValue) -> Result<f64, JsError> {
        if let JsValue::Number(n) = value {
            return Ok(*n);
        }
        let value = self.export(value).map_err(|e| self.error_from(e))?;
        value
            .get::<Coerced<f64>>()
            .map(|c| c.0)
            .map_err(|e| self.error_from(e))
    }

    fn array_length(&self, array: &JsObject) -> Result<usize, JsError> {
        let value = self.export_obj(array).map_err(|e| self.error_from(e))?;
        match value.as_array() {
            Some(arr) => Ok(arr.len()),
            None => Err(JsError::Engine("expected an array".into())),
        }
    }

    fn array_get(&self, array: &JsObject, index: usize) -> Result<JsValue, JsError> {
        let value = self.export_obj(array).map_err(|e| self.error_from(e))?;
        let Some(arr) = value.as_array() else {
            return Err(JsError::Engine("expected an array".into()));
        };
        arr.get::<Value<'js>>(index)
            .map(|v| self.import(v))
            .map_err(|e| self.error_from(e))
    }

    fn pump_jobs(&self) -> JobsOutcome {
        let mut out = JobsOutcome::default();
        while self.ctx.execute_pending_job() {
            out.executed += 1;
            if self.ctx.has_exception() {
                let caught = self.ctx.catch();
                out.errors.push(self.exception_from(caught));
            }
        }
        out
    }

    fn eval_module(&self, source: &str, url: &str) -> Result<JsValue, JsError> {
        // Declare (compile), set `import.meta.url`, then evaluate. Setting
        // meta on a declared-but-not-yet-evaluated module is the 0.12.0 path
        // that also carries `import.meta.url` into statically-imported
        // (nested) modules loaded by the loader (see ADR-0004).
        let module =
            Module::declare(self.ctx.clone(), url, source).map_err(|e| self.error_from(e))?;
        if let Ok(meta) = module.meta() {
            let _ = meta.set("url", url);
        }
        let (_evaluated, promise) = module.eval().map_err(|e| self.error_from(e))?;
        Ok(self.import(promise.into_value()))
    }

    fn promise_state(&self, value: &JsValue) -> Option<PromiseState> {
        let JsValue::Object(o) = value else {
            return None;
        };
        let value = self.export_obj(o).ok()?;
        let promise = value.as_promise()?;
        Some(match promise.state() {
            rquickjs::promise::PromiseState::Pending => PromiseState::Pending,
            rquickjs::promise::PromiseState::Resolved => PromiseState::Fulfilled,
            rquickjs::promise::PromiseState::Rejected => PromiseState::Rejected,
        })
    }

    fn promise_rejection(&self, value: &JsValue) -> Option<JsError> {
        let JsValue::Object(o) = value else {
            return None;
        };
        let value = self.export_obj(o).ok()?;
        let promise = value.as_promise()?;
        // On a rejected promise `result` rethrows the reason into the context
        // and reports `Error::Exception`; `error_from` then catches it.
        match promise.result::<Value<'_>>()? {
            Ok(_) => None,
            Err(e) => Some(self.error_from(e)),
        }
    }
}

/// Adapts a neutral [`ModuleSource`] into rquickjs's module `Resolver`.
struct SourceResolver(Rc<dyn ModuleSource>);

impl Resolver for SourceResolver {
    fn resolve<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        self.0
            .resolve(base, name)
            .map_err(|e| Exception::throw_message(ctx, &e))
    }
}

/// Adapts a neutral [`ModuleSource`] into rquickjs's module `Loader`. Loads
/// the source text (blocking on the net runtime), declares the module, and
/// stamps `import.meta.url` before returning it to the engine's linker.
struct SourceLoader(Rc<dyn ModuleSource>);

impl Loader for SourceLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        let text = self
            .0
            .load(name)
            .map_err(|e| Exception::throw_message(ctx, &e))?;
        let module = Module::declare(ctx.clone(), name, text)?;
        if let Ok(meta) = module.meta() {
            let _ = meta.set("url", name);
        }
        Ok(module)
    }
}

/// Splits a caught exception value into `(name, message, stack)`.
///
/// A thrown non-`Error` (`throw "boom"`, `throw {}`) has no name and no stack,
/// so it degrades to its string coercion — the same text the old single-string
/// rendering produced.
fn split_exception(caught: &Value<'_>) -> (Option<String>, String, Vec<StackFrame>) {
    let Some(exception) = caught.as_exception() else {
        return (None, render_thrown_value(caught), Vec::new());
    };
    let message = exception.message().unwrap_or_else(|| "<no message>".into());
    let stack = exception.stack().map_or_else(Vec::new, |s| parse_stack(&s));
    (exception_name(exception), message, stack)
}

/// Renders a thrown non-`Error` value (`throw "boom"`, `throw {}`).
///
/// `ToString` **throws** on a symbol, and the resulting `TypeError` would be
/// left pending on the context for whatever inspects it next to pick up and
/// blame on unrelated script (the hazard `QuickScope::as_host_object`
/// documents). So a symbol is named directly, and any other coercion failure
/// clears what it raised.
fn render_thrown_value(caught: &Value<'_>) -> String {
    if let Some(symbol) = caught.as_symbol() {
        let description = symbol
            .description()
            .ok()
            .filter(|d| !d.is_undefined())
            .and_then(|d| d.as_string().map(string_to_lossy))
            .unwrap_or_default();
        return format!("Symbol({description})");
    }
    let ctx = caught.ctx().clone();
    match caught.clone().get::<Coerced<String>>() {
        Ok(text) => text.0,
        Err(_) => {
            if ctx.has_exception() {
                let _ = ctx.catch();
            }
            "<unrenderable exception>".into()
        }
    }
}

/// The exception's `name` (`"TypeError"`), inherited from its prototype.
///
/// Reading it runs a property get, which a page can turn into a throwing
/// accessor; leaving *that* exception pending would surface later, blamed on
/// unrelated script (the hazard `QuickScope::as_host_object` documents), so it
/// is swallowed here.
fn exception_name<'js>(exception: &Exception<'js>) -> Option<String> {
    let ctx = exception.ctx().clone();
    match exception.get::<_, Option<Coerced<String>>>("name") {
        Ok(name) => name.map(|c| c.0).filter(|n| !n.is_empty()),
        Err(_) => {
            if ctx.has_exception() {
                let _ = ctx.catch();
            }
            None
        }
    }
}
