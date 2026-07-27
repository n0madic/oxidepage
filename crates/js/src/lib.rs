//! JavaScript engine abstraction (design doc §3.3, §5.3).
//!
//! All DOM bindings target the narrow [`JsEngine`] / [`JsRealm`] / [`JsScope`]
//! traits rather than a concrete engine, keeping a V8 backend viable behind a
//! feature flag later. The traits are deliberately minimal: host functions,
//! host objects with an opaque `(tag, data)` payload, exceptions, job-queue
//! pumping, interrupts, and memory limits — everything else (prototype
//! wiring, wrapper caches, WebIDL conversions) lives on the Rust side in the
//! `bindings` crate.
//!
//! Reentrancy model: engine APIs are only usable inside a *scope* (an entered
//! realm). Host callbacks receive the active scope as `&dyn JsScope`, so
//! nested engine calls made from inside a callback reuse the active engine
//! context instead of re-entering the realm (which would deadlock or panic
//! in most engines, QuickJS included).

pub mod error;
pub mod quickjs;
pub mod value;

use std::any::Any;
use std::rc::Rc;

pub use error::{JsError, JsThrow};
pub use quickjs::{QuickJsEngine, QuickJsRealm};
pub use value::{JsObject, JsValue};

/// A neutral ES module source the engine's module loader delegates to.
///
/// Engine-agnostic: the `page` crate implements it over the net stack, so the
/// `js` crate stays free of any HTTP dependency. `resolve` and `load` run
/// synchronously on the page thread (the implementation may block on the net
/// runtime; tokio workers deliver the bytes).
pub trait ModuleSource {
    /// Resolves `specifier` relative to the importing module's URL
    /// (`referrer`), returning the absolute module URL that keys the module.
    fn resolve(&self, referrer: &str, specifier: &str) -> Result<String, String>;
    /// Loads the source text of an already-resolved module URL.
    fn load(&self, url: &str) -> Result<String, String>;
}

/// The settled state of a JS `Promise`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromiseState {
    Pending,
    Fulfilled,
    Rejected,
}

/// A JavaScript engine backend, producing realms.
pub trait JsEngine: 'static {
    type Realm: JsRealm;
    fn new_realm(&self, opts: RealmOptions) -> Result<Self::Realm, JsError>;
}

/// Per-realm resource limits (design doc §8 "JS containment").
#[derive(Clone, Copy, Debug, Default)]
pub struct RealmOptions {
    /// Hard cap on the realm's heap, in bytes.
    pub memory_limit: Option<usize>,
    /// Native stack limit, in bytes.
    pub max_stack_size: Option<usize>,
    /// GC trigger threshold, in bytes.
    pub gc_threshold: Option<usize>,
}

/// Outcome of draining the engine's promise-job queue.
#[derive(Debug, Default)]
pub struct JobsOutcome {
    /// Number of jobs executed (including ones that threw).
    pub executed: usize,
    /// Rendered messages of exceptions thrown by jobs.
    pub errors: Vec<String>,
}

impl JobsOutcome {
    pub fn merge(&mut self, other: JobsOutcome) {
        self.executed += other.executed;
        self.errors.extend(other.errors);
    }
}

/// Arguments of a host-function invocation, already converted to
/// engine-neutral values.
pub struct HostCall {
    /// The `this` value (for constructors: the `new.target` function).
    pub this: JsValue,
    pub args: Vec<JsValue>,
}

impl HostCall {
    /// The nth argument, `undefined` when absent (WebIDL semantics).
    #[must_use]
    pub fn arg(&self, index: usize) -> JsValue {
        self.args.get(index).cloned().unwrap_or(JsValue::Undefined)
    }
}

/// A host function callable from JS. Errors become JS exceptions.
pub type HostFn = Rc<dyn Fn(&dyn JsScope, HostCall) -> Result<JsValue, JsThrow>>;

/// A property definition for [`JsScope::define_property`]
/// (WebIDL-style: methods and accessors are non-enumerable by default,
/// which is why plain `set` is not enough).
pub enum PropertyDef<'a> {
    Value {
        value: &'a JsValue,
        writable: bool,
        enumerable: bool,
        configurable: bool,
    },
    Accessor {
        /// Must be a function value if present.
        getter: Option<&'a JsValue>,
        /// Must be a function value if present.
        setter: Option<&'a JsValue>,
        enumerable: bool,
        configurable: bool,
    },
}

/// An entered realm: the only way to touch JS values.
///
/// Object-safe so host callbacks can receive `&dyn JsScope` regardless of
/// the engine backend.
pub trait JsScope {
    /// Host state installed via [`JsRealm::set_state`] (the bindings'
    /// per-page state; callbacks retrieve it from the scope instead of
    /// capturing it, which keeps JS→Rust reference cycles impossible).
    fn state(&self) -> Option<Rc<dyn Any>>;

    fn eval(&self, source: &str, filename: &str) -> Result<JsValue, JsError>;

    fn global(&self) -> JsObject;
    fn new_object(&self) -> Result<JsObject, JsError>;
    fn new_object_with_proto(&self, proto: Option<&JsObject>) -> Result<JsObject, JsError>;
    fn new_array(&self, items: &[JsValue]) -> Result<JsObject, JsError>;

    /// Creates an `ArrayBuffer` holding a copy of `bytes`.
    ///
    /// Direct, because the alternative is not: building one boxed `JsValue`
    /// per byte and a JS array of that length before converting is tens of
    /// megabytes of transient allocation for a 10 MB download.
    fn new_array_buffer(&self, bytes: &[u8]) -> Result<JsObject, JsError>;

    /// Creates a host function with the given `name` and `length`.
    fn new_function(&self, name: &str, length: u32, f: HostFn) -> Result<JsObject, JsError>;

    /// Creates a constructor function whose `prototype` property is `proto`
    /// (and `proto.constructor` points back at it). When invoked with `new`,
    /// `f` receives the `new.target` function as `this` and must return an
    /// object; its prototype is fixed up for subclassing per spec.
    fn new_constructor(
        &self,
        name: &str,
        length: u32,
        proto: &JsObject,
        f: HostFn,
    ) -> Result<JsObject, JsError>;

    /// Creates a host object carrying an opaque `(tag, data)` payload, with
    /// the given prototype. When the object is garbage-collected the payload
    /// is reported through [`JsRealm::take_finalized`].
    fn new_host_object(
        &self,
        proto: Option<&JsObject>,
        tag: u32,
        data: u64,
    ) -> Result<JsObject, JsError>;

    /// The `(tag, data)` payload of a host object, `None` for anything else.
    fn host_payload(&self, value: &JsValue) -> Option<(u32, u64)>;

    fn get(&self, obj: &JsObject, key: &str) -> Result<JsValue, JsError>;
    fn set(&self, obj: &JsObject, key: &str, value: &JsValue) -> Result<(), JsError>;
    fn define_property(
        &self,
        obj: &JsObject,
        key: &str,
        prop: PropertyDef<'_>,
    ) -> Result<(), JsError>;
    fn set_prototype(&self, obj: &JsObject, proto: Option<&JsObject>) -> Result<(), JsError>;

    /// Calls `function` with the given `this` and arguments.
    fn call(
        &self,
        function: &JsValue,
        this: &JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JsError>;

    fn is_function(&self, value: &JsValue) -> bool;
    fn is_array(&self, value: &JsValue) -> bool;
    /// JS `===` (for object identity; primitives compare structurally).
    fn strict_equals(&self, a: &JsValue, b: &JsValue) -> bool;

    /// JS `ToString` coercion.
    fn coerce_string(&self, value: &JsValue) -> Result<String, JsError>;
    /// JS `ToNumber` coercion.
    fn coerce_number(&self, value: &JsValue) -> Result<f64, JsError>;

    fn array_length(&self, array: &JsObject) -> Result<usize, JsError>;
    fn array_get(&self, array: &JsObject, index: usize) -> Result<JsValue, JsError>;

    /// Drains the engine job queue from inside the scope (microtask
    /// checkpoints that run while a scope is active).
    fn pump_jobs(&self) -> JobsOutcome;

    /// Declares and evaluates an ES module with the given `url` (its
    /// `import.meta.url` and the base for resolving its static imports).
    /// Static imports resolve through the loader installed via
    /// [`JsRealm::set_module_loader`]. Returns the module's evaluation
    /// promise (inspect it with [`JsScope::promise_state`]).
    fn eval_module(&self, source: &str, url: &str) -> Result<JsValue, JsError>;

    /// The settled state of `value` if it is a Promise, otherwise `None`.
    fn promise_state(&self, value: &JsValue) -> Option<PromiseState>;

    /// The rejection reason of an already-rejected promise, rendered like an
    /// uncaught exception (message plus stack when the reason is an `Error`).
    /// `None` unless `value` is a Promise in the `Rejected` state.
    fn promise_rejection(&self, value: &JsValue) -> Option<JsError>;
}

/// A JS realm (global environment + heap budget + job queue).
pub trait JsRealm: 'static {
    /// Enters the realm, providing scoped access to JS values.
    ///
    /// Must not be called re-entrantly from inside a host callback; use the
    /// scope the callback received instead.
    fn with_scope<T>(&self, f: impl FnOnce(&dyn JsScope) -> T) -> T;

    /// Installs the host state returned by [`JsScope::state`].
    fn set_state(&self, state: Rc<dyn Any>);
    fn state(&self) -> Option<Rc<dyn Any>>;

    /// Installs the ES module loader. Modules evaluated via
    /// [`JsScope::eval_module`], and their static imports, resolve and load
    /// through `source`.
    fn set_module_loader(&self, source: Rc<dyn ModuleSource>);

    /// Drains the engine job queue (promise reactions).
    fn pump_jobs(&self) -> JobsOutcome;
    fn has_pending_jobs(&self) -> bool;

    /// Runs a full garbage collection cycle. Host-object finalizations it
    /// causes become visible through [`JsRealm::take_finalized`].
    fn run_gc(&self);

    /// Takes the `(tag, data)` payloads of host objects finalized since the
    /// last call (the bindings' pin bookkeeping consumes this).
    fn take_finalized(&self) -> Vec<(u32, u64)>;

    /// Installs an interrupt callback, polled during execution; returning
    /// `true` aborts the running script.
    fn set_interrupt(&self, callback: Option<Box<dyn FnMut() -> bool>>);

    /// Installs a promise-rejection tracker. The callback receives the
    /// rendered rejection reason and `is_handled` (`false` when a rejection
    /// becomes unhandled, `true` when a previously-unhandled rejection gets
    /// a handler attached after the fact).
    fn set_rejection_tracker(&self, callback: Option<Box<dyn Fn(String, bool)>>);

    fn set_memory_limit(&self, bytes: usize);

    /// Bytes currently allocated by the engine.
    fn memory_used(&self) -> i64;
}
