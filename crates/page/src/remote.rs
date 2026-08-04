//! The embedder-facing half of the remote object model (ADR-0030).
//!
//! Everything here takes and returns **owned, `Send`** data. The live values
//! stay behind in `WorldState`'s `ObjectStore`, named by `u64`, because a
//! `JsValue` is `!Send` and must drop before the realm.
//!
//! Nothing in this module knows what CDP is. The shapes match its vocabulary —
//! as `ScreenshotOptions` and `NavigationEventKind` already do — but they are
//! plain Rust, and the protocol crate does the JSON.

use oxidepage_bindings::remote::{
    EvaluationResult, ExceptionDetails, PropertyDescriptor, RemoteObject, RemoteOptions,
    RemoteType, describe, describe_exception, describe_properties,
};
use oxidepage_js::{JsValue, PromiseState};

use crate::Page;

/// Most properties one `get_properties` call reports.
///
/// A driver enumerating a large array otherwise mints a handle per element,
/// which is both a round trip and a pin.
pub const MAX_PROPERTIES: usize = 1000;

/// How an evaluation should run.
#[derive(Clone, Debug, Default)]
pub struct EvaluateOptions {
    /// Serialize the result instead of minting a handle for it.
    pub by_value: bool,
    /// If the result is a promise, settle it and report what it settled to.
    pub await_promise: bool,
    /// The release group any minted handle joins.
    pub group: Option<String>,
    /// Filename shown in stack traces.
    pub source_url: Option<String>,
}

/// One argument to [`Page::call_function_on`]: either a live handle or a
/// literal.
#[derive(Clone, Debug, Default)]
pub struct CallArgument {
    /// A handle previously minted by this page.
    pub object_id: Option<u64>,
    /// A JSON literal, parsed by the realm's own `JSON.parse`.
    pub value_json: Option<String>,
    /// A primitive JSON cannot spell — `NaN`, `Infinity`, `-Infinity`, `-0`,
    /// `1n` — passed as **source** and evaluated.
    ///
    /// It has to be source: there is no JSON literal for any of them, so the
    /// obvious encoding (a JSON string) delivers the *string* `"NaN"` to the
    /// page rather than the number. Every driver sends this form for those
    /// values, so getting it wrong is not an edge case.
    pub unserializable: Option<String>,
}

/// Why a call could not even be attempted — as opposed to a script that threw,
/// which is an `exception` inside [`EvaluationResult`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteError {
    /// No live value has this id: it was released, or its document is gone.
    NoSuchObject(u64),
    /// The handle table is full (`MAX_REMOTE_OBJECTS`).
    OutOfHandles,
    /// The value is not callable / not a promise / not an object.
    WrongType(String),
    /// An argument's JSON could not be parsed.
    BadArgument(String),
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteError::NoSuchObject(id) => write!(f, "Could not find object with given id: {id}"),
            RemoteError::OutOfHandles => write!(f, "Too many remote objects are retained"),
            RemoteError::WrongType(detail) => write!(f, "{detail}"),
            RemoteError::BadArgument(detail) => write!(f, "Invalid call argument: {detail}"),
        }
    }
}

impl Page {
    /// The id of the current document's execution context.
    ///
    /// Bumped on every commit, so a driver holding an old one can tell its
    /// handles are dead without probing each.
    #[must_use]
    pub fn execution_context_id(&self) -> u64 {
        self.state.context_id.get()
    }

    /// How many handles are currently retained. Diagnostic.
    #[must_use]
    pub fn retained_object_count(&self) -> usize {
        self.worlds
            .all()
            .iter()
            .filter_map(|w| w.state())
            .map(|state| state.remote_objects.borrow().len())
            .sum()
    }

    /// Evaluates `source` in the main world, CDP's `Runtime.evaluate`.
    pub fn evaluate(&self, source: &str, options: &EvaluateOptions) -> EvaluationResult {
        self.evaluate_in(None, source, options)
            .unwrap_or_else(|_| unreachable!("the main world always exists"))
    }

    /// Evaluates in the world a `Runtime.ExecutionContextId` names, or the main
    /// world when `context_id` is `None`.
    ///
    /// `Err` when no live world has that id — which is the point of routing by
    /// the monotonic context id rather than a recycled world index: an id from
    /// before a commit is a clean error, never a silent hit on a live world
    /// (ADR-0033 D10).
    pub fn evaluate_in(
        &self,
        context_id: Option<u64>,
        source: &str,
        options: &EvaluateOptions,
    ) -> Result<EvaluationResult, String> {
        let world = match context_id {
            None => oxidepage_bindings::MAIN_WORLD,
            Some(id) => self
                .world_id_by_context(id)
                .ok_or_else(|| String::from("Cannot find context with specified id"))?,
        };
        Ok(self.evaluate_in_world(world, source, options))
    }

    fn evaluate_in_world(
        &self,
        world: oxidepage_bindings::WorldId,
        source: &str,
        options: &EvaluateOptions,
    ) -> EvaluationResult {
        let filename = options
            .source_url
            .clone()
            .unwrap_or_else(|| String::from("oxidepage:evaluate"));
        // Boxed: `ExceptionDetails` is the larger variant by far, and every
        // successful evaluation would otherwise pay for its size.
        let outcome = self
            .with_cx_in(world, |cx| match cx.scope.eval(source, &filename) {
                Ok(value) => Ok(value),
                Err(error) => Err(Box::new(describe_exception(
                    cx,
                    &error,
                    options.group.as_deref(),
                ))),
            })
            .unwrap_or_else(|| {
                Err(Box::new(ExceptionDetails {
                    text: String::from("the execution context could not be entered"),
                    line: 0,
                    column: 0,
                    url: String::new(),
                    exception: None,
                }))
            });

        let result = match outcome {
            Ok(value) => self.finish_value(world, value, options),
            Err(exception) => EvaluationResult {
                result: RemoteObject::undefined(),
                exception: Some(*exception),
            },
        };
        // Same contract as `Page::eval`: microtasks and any task the script
        // queued run before the caller sees the answer.
        self.run_until_stalled();
        result
    }

    /// Calls `declaration` — a function *expression* — with `this` bound to
    /// `object_id`. CDP's `Runtime.callFunctionOn`.
    ///
    /// This is the command both drivers lean on hardest: they ship their own
    /// helper functions and pass handles, rather than building source strings.
    pub fn call_function_on(
        &self,
        declaration: &str,
        object_id: Option<u64>,
        context_id: Option<u64>,
        args: &[CallArgument],
        options: &EvaluateOptions,
    ) -> Result<EvaluationResult, RemoteError> {
        // **The world comes from the handle**, and `executionContextId` selects
        // it when there is no handle (ADR-0033 D10). A driver's utility-world
        // handle must be called in that world: the value is a `Persistent` of
        // that runtime, would fail to restore anywhere else, and the result
        // must be filed back in the same store.
        //
        // Both present and disagreeing is an error rather than a silent
        // preference, which is Chrome's behaviour and is what stops a handle
        // being called in the wrong world.
        let from_handle = match object_id {
            Some(id) => Some(self.object_world(id)?),
            None => None,
        };
        let from_context = match context_id {
            None => None,
            Some(id) => Some(self.world_id_by_context(id).ok_or_else(|| {
                RemoteError::BadArgument(String::from("Cannot find context with specified id"))
            })?),
        };
        let world = match (from_handle, from_context) {
            (Some(handle), Some(context)) if handle != context => {
                return Err(RemoteError::BadArgument(String::from(
                    "objectId and executionContextId name different execution contexts",
                )));
            }
            (Some(world), _) | (None, Some(world)) => world,
            (None, None) => oxidepage_bindings::MAIN_WORLD,
        };
        let this_value = match object_id {
            Some(id) => Some(self.lookup(id)?),
            None => None,
        };

        let outcome = self
            .with_cx_in(world, |cx| {
                // Parenthesized so a bare `function (){}` or `async () => {}` parses
                // as an expression rather than a declaration — which is the shape
                // every driver sends.
                let factory = cx
                    .scope
                    .eval(&format!("({declaration})"), "oxidepage:callFunctionOn")
                    .map_err(|error| {
                        Ok::<_, RemoteError>(describe_exception(
                            cx,
                            &error,
                            options.group.as_deref(),
                        ))
                    });
                let function = match factory {
                    Ok(function) => function,
                    Err(Ok(exception)) => return Ok(Err(exception)),
                    Err(Err(error)) => return Err(error),
                };
                if !cx.scope.is_function(&function) {
                    return Err(RemoteError::WrongType(String::from(
                        "functionDeclaration did not evaluate to a function",
                    )));
                }

                let mut resolved = Vec::with_capacity(args.len());
                for argument in args {
                    resolved.push(self.resolve_argument(cx, argument)?);
                }

                let this = this_value.clone().unwrap_or(JsValue::Undefined);
                match cx.scope.call(&function, &this, &resolved) {
                    Ok(value) => Ok(Ok(value)),
                    Err(error) => Ok(Err(describe_exception(
                        cx,
                        &error,
                        options.group.as_deref(),
                    ))),
                }
            })
            .unwrap_or_else(|| {
                Err(RemoteError::WrongType(String::from(
                    "the execution context could not be entered",
                )))
            })?;

        let result = match outcome {
            Ok(value) => self.finish_value(world, value, options),
            Err(exception) => EvaluationResult {
                result: RemoteObject::undefined(),
                exception: Some(exception),
            },
        };
        self.run_until_stalled();
        Ok(result)
    }

    /// The own enumerable properties of a handle. CDP's `Runtime.getProperties`.
    pub fn get_properties(
        &self,
        object_id: u64,
        group: Option<&str>,
    ) -> Result<Vec<PropertyDescriptor>, RemoteError> {
        let world = self.object_world(object_id)?;
        let value = self.lookup(object_id)?;
        let properties = self
            .with_cx_in(world, |cx| {
                describe_properties(cx, &value, MAX_PROPERTIES, group)
            })
            .unwrap_or(Err(oxidepage_js::JsThrow::Type("no such world".into())));
        properties.map_err(|_| {
            RemoteError::WrongType(String::from("Object properties could not be enumerated"))
        })
    }

    /// Settles a promise handle and describes what it settled to. CDP's
    /// `Runtime.awaitPromise`.
    pub fn await_promise(
        &self,
        object_id: u64,
        options: &EvaluateOptions,
    ) -> Result<EvaluationResult, RemoteError> {
        let world = self.object_world(object_id)?;
        let value = self.lookup(object_id)?;
        if self
            .with_cx_in(world, |cx| cx.scope.promise_state(&value))
            .flatten()
            .is_none()
        {
            return Err(RemoteError::WrongType(String::from(
                "Could not find promise with given id",
            )));
        }
        Ok(self.settle_promise(world, value, options))
    }

    /// Releases one handle.
    pub fn release_object(&self, object_id: u64) -> bool {
        let Some(world) = self.shared.object_world(object_id) else {
            return false;
        };
        self.shared.forget_object(object_id);
        self.worlds
            .get(world)
            .and_then(|w| w.state())
            .is_some_and(|state| state.remote_objects.borrow_mut().release(object_id))
    }

    /// Releases every handle in a group.
    pub fn release_object_group(&self, group: &str) -> usize {
        // Sweeps **every** world: a group is a driver-side label, and a
        // driver expects `releaseObjectGroup` to take everything it tagged,
        // whichever world the handle happened to be minted in.
        let mut released = 0;
        for world in self.worlds.all() {
            let Some(state) = world.state() else { continue };
            let mut store = state.remote_objects.borrow_mut();
            let before: Vec<u64> = store.ids();
            released += store.release_group(group);
            let after: std::collections::HashSet<u64> = store.ids().into_iter().collect();
            drop(store);
            for id in before.into_iter().filter(|id| !after.contains(id)) {
                self.shared.forget_object(id);
            }
        }
        released
    }

    /// Installs a global function that reports its single string argument back
    /// to the embedder. CDP's `Runtime.addBinding`.
    ///
    /// The function is a real closure over `name`, which is what lets one
    /// native serve every binding without a per-name trampoline in JavaScript.
    pub fn add_binding(&self, name: &str) -> Result<(), String> {
        self.add_binding_in(name, None)
    }

    /// Installs a binding in one world, or in every world when `world` is
    /// `None`.
    ///
    /// The registration is remembered page-side and re-applied after every
    /// commit, because a commit rebuilds each isolated world against a fresh
    /// global (ADR-0033 D9) — without that, a driver's `exposeBinding` would
    /// vanish on the first navigation.
    pub fn add_binding_in(&self, name: &str, world: Option<&str>) -> Result<(), String> {
        Self::validate_binding_name(name)?;
        if let Some(world) = world
            && !world.is_empty()
        {
            // Naming a world that does not exist yet creates it: that is how
            // `Runtime.addBinding { executionContextName }` is used, ahead of
            // the `createIsolatedWorld` that a driver may never send.
            self.create_isolated_world(world)?;
        }
        self.shared
            .bindings()
            .push((name.to_owned(), world.map(str::to_owned)));
        self.install_binding(name, world)
    }

    /// Installs every remembered binding that applies to `world`.
    pub(crate) fn apply_bindings_to(&self, world: &str) {
        let registrations: Vec<(String, Option<String>)> = self.shared.bindings().clone();
        for (name, target) in registrations {
            let applies = match target.as_deref() {
                None => true,
                Some(t) => t == world,
            };
            if applies && let Err(error) = self.install_binding(&name, Some(world)) {
                self.hooks.report_resource_error(format!(
                    "could not reinstall binding {name:?}: {error}"
                ));
            }
        }
    }

    fn validate_binding_name(name: &str) -> Result<(), String> {
        // A name that is not a plain identifier would be set as an exotic
        // global property a page could not call anyway, and is far more likely
        // to be a driver bug than intent.
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
            || name.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(format!("Invalid binding name: {name}"));
        }
        Ok(())
    }

    /// Defines the binding function on one world's global, or on every world's.
    fn install_binding(&self, name: &str, world: Option<&str>) -> Result<(), String> {
        let targets: Vec<oxidepage_bindings::WorldId> = match world {
            Some(w) if !w.is_empty() => match self.world_id_by_name(w) {
                Some(id) => vec![id],
                None => return Err(format!("no such world: {w}")),
            },
            _ => self.world_ids(),
        };
        for id in targets {
            let result = self
                .with_cx_in(id, |cx| Self::define_binding(cx, name))
                .unwrap_or_else(|| Err("the world could not be entered".to_owned()));
            result?;
        }
        Ok(())
    }

    fn define_binding(cx: &oxidepage_bindings::BindCx<'_>, name: &str) -> Result<(), String> {
        let owned = name.to_owned();
        {
            let bound = owned.clone();
            let function = cx
                .scope
                .new_function(
                    &owned,
                    1,
                    std::rc::Rc::new(move |scope, call| {
                        let payload = call.args.first().map_or_else(String::new, |value| {
                            scope.coerce_string(value).unwrap_or_default()
                        });
                        let state = oxidepage_bindings::cx::world_state(scope)?;
                        let mut queue = state.page.binding_calls.borrow_mut();
                        // Bounded: a page that calls its binding in a loop must
                        // not grow this without limit while the driver is busy.
                        if queue.len() >= MAX_BINDING_CALLS {
                            queue.pop_front();
                        }
                        queue.push_back(oxidepage_bindings::BindingCall {
                            name: bound.clone(),
                            payload,
                            // The *calling* world, not the main one: a
                            // binding installed in a utility world reports
                            // that world's context to the driver.
                            context_id: state.context_id.get(),
                        });
                        Ok(JsValue::Undefined)
                    }),
                )
                .map_err(|e| e.to_string())?;
            let global = cx.scope.global();
            cx.scope
                .set(&global, &owned, &JsValue::Object(function))
                .map_err(|e| e.to_string())
        }
    }

    /// Takes the binding payloads the page has produced since the last call.
    #[must_use]
    pub fn drain_binding_calls(&self) -> Vec<oxidepage_bindings::BindingCall> {
        self.state
            .page
            .binding_calls
            .borrow_mut()
            .drain(..)
            .collect()
    }

    // === internals ===

    /// The world whose store holds `object_id`.
    fn object_world(&self, object_id: u64) -> Result<oxidepage_bindings::WorldId, RemoteError> {
        self.shared
            .object_world(object_id)
            .ok_or(RemoteError::NoSuchObject(object_id))
    }

    fn lookup(&self, object_id: u64) -> Result<JsValue, RemoteError> {
        let world = self.object_world(object_id)?;
        self.worlds
            .get(world)
            .and_then(|w| w.state())
            .and_then(|state| state.remote_objects.borrow().get(object_id))
            .ok_or(RemoteError::NoSuchObject(object_id))
    }

    fn resolve_argument(
        &self,
        cx: &oxidepage_bindings::cx::BindCx<'_>,
        argument: &CallArgument,
    ) -> Result<JsValue, RemoteError> {
        if let Some(source) = &argument.unserializable {
            // Restricted to the spellings CDP actually defines for
            // `unserializableValue`. This string comes off the wire, and
            // evaluating it unfiltered would make the member an eval hole in a
            // command whose whole point is passing *data*.
            if !is_unserializable_literal(source) {
                return Err(RemoteError::BadArgument(format!(
                    "unserializableValue must be NaN, Infinity, -Infinity, -0 or a BigInt \
                     literal, got {source}"
                )));
            }
            return cx
                .scope
                .eval(source, "oxidepage:callArgument")
                .map_err(|e| RemoteError::BadArgument(e.to_string()));
        }
        if let Some(id) = argument.object_id {
            // An argument handle must belong to the world the call runs in: a
            // value cannot cross a world (ADR-0033 D5), so a foreign handle is
            // a `BadArgument` rather than a value that would fail to restore
            // halfway through the call.
            let world = self
                .shared
                .object_world(id)
                .ok_or(RemoteError::NoSuchObject(id))?;
            if world != cx.state.id {
                return Err(RemoteError::BadArgument(format!(
                    "object {id} belongs to a different JavaScript world"
                )));
            }
            return cx
                .state
                .remote_objects
                .borrow()
                .get(id)
                .ok_or(RemoteError::NoSuchObject(id));
        }
        let Some(json) = &argument.value_json else {
            // CDP's `CallArgument` with neither member set means `undefined`.
            return Ok(JsValue::Undefined);
        };
        // Parsed by the realm's own `JSON.parse` so the resulting objects
        // belong to the realm and carry its prototypes — an object built in
        // Rust would have none.
        let global = cx.scope.global();
        let json_ns = cx
            .scope
            .get(&global, "JSON")
            .map_err(|e| RemoteError::BadArgument(e.to_string()))?;
        let json_object = json_ns
            .as_object()
            .ok_or_else(|| RemoteError::BadArgument(String::from("JSON is not an object")))?;
        let parse = cx
            .scope
            .get(json_object, "parse")
            .map_err(|e| RemoteError::BadArgument(e.to_string()))?;
        cx.scope
            .call(&parse, &json_ns, &[JsValue::String(json.clone())])
            .map_err(|e| RemoteError::BadArgument(e.to_string()))
    }

    /// Describes `value`, settling it first if it is a promise and the caller
    /// asked for that.
    /// Turns a raw value into an `EvaluationResult` **in the world it came
    /// from**.
    ///
    /// The world is not decoration. Describing a world-B value from world A
    /// would file the handle in A's object store, and teardown then drops a
    /// `Persistent` of B's runtime after that runtime is gone — the
    /// `JS_FreeRuntime` abort (ADR-0033 D4), found by
    /// `dropping_a_page_with_live_worlds_is_clean`.
    fn finish_value(
        &self,
        world: oxidepage_bindings::WorldId,
        value: JsValue,
        options: &EvaluateOptions,
    ) -> EvaluationResult {
        let is_promise = self
            .with_cx_in(world, |cx| cx.scope.promise_state(&value).is_some())
            .unwrap_or(false);
        if options.await_promise && is_promise {
            return self.settle_promise(world, value, options);
        }
        self.described(world, &value, options)
    }

    /// [`Page::describe`] as an `EvaluationResult`, an exhausted handle table
    /// becoming an exception rather than a malformed result.
    fn described(
        &self,
        world: oxidepage_bindings::WorldId,
        value: &JsValue,
        options: &EvaluateOptions,
    ) -> EvaluationResult {
        match self.describe(world, value, options) {
            Ok(result) => EvaluationResult {
                result,
                exception: None,
            },
            Err(error) => EvaluationResult {
                result: RemoteObject::undefined(),
                exception: Some(ExceptionDetails {
                    text: error.to_string(),
                    ..ExceptionDetails::default()
                }),
            },
        }
    }

    /// Runs the event loop until `promise` settles, then describes the outcome.
    ///
    /// The loop has to run: a promise resolved by a timer or a fetch settles in
    /// a *later* task, and simply reading its state here would report `pending`
    /// forever. The `settle` budget bounds it, so a promise that never resolves
    /// is reported as still pending rather than hanging the connection.
    fn settle_promise(
        &self,
        world: oxidepage_bindings::WorldId,
        promise: JsValue,
        options: &EvaluateOptions,
    ) -> EvaluationResult {
        let deadline = std::time::Instant::now() + AWAIT_PROMISE_BUDGET;
        loop {
            let state = self
                .with_cx_in(world, |cx| cx.scope.promise_state(&promise))
                .flatten();
            match state {
                Some(PromiseState::Fulfilled) | Some(PromiseState::Rejected) => break,
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                // Still pending after the budget: report the promise itself
                // rather than hanging the lane on one that never settles.
                return self.described(world, &promise, options);
            }
            self.settle(AWAIT_PROMISE_STEP);
        }

        // Every read below is in the promise's **own** world: the value is a
        // `Persistent` of that runtime, so reading it from the main world
        // fails to restore and reports `undefined` for a promise that really
        // did fulfil.
        let rejection = self
            .with_cx_in(world, |cx| cx.scope.promise_rejection(&promise))
            .flatten();
        if let Some(error) = rejection {
            let exception = self
                .with_cx_in(world, |cx| {
                    describe_exception(cx, &error, options.group.as_deref())
                })
                .unwrap_or_default();
            return EvaluationResult {
                result: RemoteObject::undefined(),
                exception: Some(exception),
            };
        }

        // A fulfilled promise's value is read through `then`, because the
        // engine exposes state but not the settled value directly.
        let value = self.with_cx_in(world, |cx| {
            let global = JsValue::Object(cx.scope.global());
            let capture = cx
                .scope
                .eval(
                    "(p) => { let out; p.then(v => { out = v; }); return () => out; }",
                    "oxidepage:awaitPromise",
                )
                .ok()?;
            let getter = cx
                .scope
                .call(&capture, &global, std::slice::from_ref(&promise))
                .ok()?;
            Some(getter)
        });
        let Some(Some(getter)) = value else {
            return EvaluationResult {
                result: RemoteObject::undefined(),
                exception: None,
            };
        };
        // The `then` reaction is a microtask; one turn of the loop runs it.
        self.settle(AWAIT_PROMISE_STEP);
        let settled = self
            .with_cx_in(world, |cx| {
                let global = JsValue::Object(cx.scope.global());
                cx.scope
                    .call(&getter, &global, &[])
                    .unwrap_or(JsValue::Undefined)
            })
            .unwrap_or(JsValue::Undefined);
        self.described(world, &settled, options)
    }

    /// Describes `value`, failing if it needed a handle and none was left.
    ///
    /// A `RemoteObject` with no `objectId` for something that is not a
    /// primitive is handle-shaped but names nothing — precisely the outcome
    /// `MAX_REMOTE_OBJECTS` exists to prevent. The caller turns this into an
    /// exception the driver can read.
    fn describe(
        &self,
        world: oxidepage_bindings::WorldId,
        value: &JsValue,
        options: &EvaluateOptions,
    ) -> Result<RemoteObject, RemoteError> {
        let object = self
            .with_cx_in(world, |cx| {
                describe(
                    cx,
                    value,
                    RemoteOptions {
                        by_value: options.by_value,
                        group: options.group.as_deref(),
                    },
                )
            })
            .ok_or(RemoteError::OutOfHandles)?;
        // Only the kinds that *have* handles. A primitive legitimately arrives
        // with no `objectId`: `undefined` carries nothing at all, and a symbol
        // is a description and nothing else — reading "handle-shaped but empty"
        // off the absence alone reported `Symbol('x')` as a full object table.
        let needs_handle = !options.by_value
            && object.object_id.is_none()
            && object.value_json.is_none()
            && object.unserializable.is_none()
            && matches!(object.kind, Some(RemoteType::Object | RemoteType::Function));
        if needs_handle {
            return Err(RemoteError::OutOfHandles);
        }
        Ok(object)
    }
}

/// Whether `source` is one of CDP's `unserializableValue` spellings.
///
/// An allow-list rather than a sanitizer: the value arrives from the driver and
/// is evaluated as source, so anything not on this list must be refused rather
/// than cleaned up.
fn is_unserializable_literal(source: &str) -> bool {
    matches!(source, "NaN" | "Infinity" | "-Infinity" | "-0")
        || source.strip_suffix('n').is_some_and(|digits| {
            let digits = digits.strip_prefix('-').unwrap_or(digits);
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        })
}

/// Longest an `awaitPromise` waits before reporting the promise still pending.
///
/// Bounded on purpose: a promise that never settles must not hold the command
/// lane — and therefore the driver's whole session — indefinitely.
const AWAIT_PROMISE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// One turn of the loop while waiting for a promise.
const AWAIT_PROMISE_STEP: std::time::Duration = std::time::Duration::from_millis(20);

/// Most binding payloads buffered before the oldest is dropped.
const MAX_BINDING_CALLS: usize = 1024;
