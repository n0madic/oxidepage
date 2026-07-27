# ADR-0025: Dialogs and structured page events

- Status: accepted
- Date: 2026-07-27
- Builds on: ADR-0022, whose bounded, drained `NavigationEvent` stream is the
  shape the three streams here copy.
- Constrained by: ADR-0004 (the page thread's single blocking wait, which
  stage 5 must turn into a `Select` before a dialog can block on a driver).

## Context

Stage 3 of `docs/automation-roadmap.md` closes two gaps.

**`window.alert`, `confirm` and `prompt` did not exist.** Not stubbed —
absent, with zero references anywhere in the repository. A page that called one
died on a `ReferenceError` before any protocol layer could have helped it. This
was the cheapest fix in the roadmap with the largest "real sites stop dying"
payoff.

**Console messages and script errors were flattened to strings at the engine
boundary.** `ConsoleMessage` was `{ level, message }`, so the *values* of the
logged arguments were gone by the time the embedder saw them, and an object
argument printed as `[object Object]`. `Page::drain_errors` returned
`Vec<String>`, so the stack `render_exception` had already built survived only
as an unparsed blob glued onto the message with a newline — and the exception's
`name` was thrown away entirely, which is why a `TypeError` reached the CLI as a
bare sentence.

Those two payloads are exactly what CDP's `Runtime.consoleAPICalled` /
`Runtime.exceptionThrown` and Puppeteer's `page.on('console')` /
`page.on('pageerror')` will need at stage 6, and both are worth having in the
CLI today.

The roadmap's stage-map row for stage 3 said "ADR: no". That was wrong the
moment an embedder-facing dialog hook entered the picture: this changes the
public API shape in six places (see Consequences), which `CLAUDE.md` requires an
ADR for. The row is corrected in the same commit.

## Decision

### The dialog hook is synchronous, and that *is* the spec's "pause"

`alert`/`confirm`/`prompt` return a value **inline to JS**. They therefore
cannot be a drained task source the way `PageState::pending_navigation` is
(`crates/bindings/src/state.rs:1186`) — the answer has to come back while the
calling script is still on the stack. So `HostHooks` gains a synchronous method
consulted with JS on the stack (`crates/bindings/src/state.rs:46`):

```rust
fn run_dialog(&self, request: DialogRequest) -> DialogResponse;
```

`LoopHooks::run_dialog` (`crates/page/src/lib.rs:669`) resolves the answer from
the embedder handler — auto-dismiss when none is installed — records a
`DialogEvent` carrying **both the ask and the answer**, and returns.

The page pauses for the dialog's duration *for free*: the handler runs
synchronously on the page thread, so no timer, frame callback or network event
can interleave. That is HTML's "pause" behavior with no machinery at all, and
`crates/page/tests/dialogs.rs::nothing_runs_while_a_dialog_is_open` pins it with
a handler that sleeps past a timer's deadline.

Auto-dismiss is not a stub hiding behind P6. The API is present, its return
values are documented and observable, every call lands in a stream, and a
handler can answer differently. It is also the policy both Puppeteer and
Playwright apply when no `dialog` listener is attached, and HTML itself sanctions
it ("the user agent may abort these steps" — the user-has-disabled-dialogs path).

**Stage 6 mapping.** `DialogRequest` is `Page.javascriptDialogOpening`;
`DialogResponse` is `Page.handleJavaScriptDialog { accept, promptText }`;
`DialogEvent` carries both halves, so the stream also covers
`Page.javascriptDialogClosed`. In real CDP the renderer *blocks* between those
two messages. That variant becomes possible when stage 5 turns `settle`'s single
blocking wait into a `crossbeam::Select` over `net_rx` **and** a command channel
(`crates/page/src/lib.rs`, ADR-0004): the handler signature does not change —
the stage-5 handler simply blocks on the command channel instead of answering
from a closure. **Recorded for stage 5:** that receive needs a disconnect or
timeout path, because the `ScriptBudget` cannot save a page whose block is in
Rust rather than in JS.

### Reentrancy: plain data in, plain data out

The handler runs with JS on the stack and `RefCell` borrows potentially held on
dom, style and layout — the same hazard as the "reflow must never re-enter JS"
invariant. Three things make it safe:

- **The signature carries no capabilities.** `DialogHandler` is
  `Rc<dyn Fn(&DialogRequest) -> DialogResponse>`
  (`crates/bindings/src/dialog.rs`). It receives owned data and returns owned
  data; reaching the `Page` requires deliberately capturing a `Weak<Page>`.
- **The call site drops its borrow first.** `imp::window::open_dialog`
  (`crates/bindings/src/imp/window.rs`) reads the document URL into a `String`,
  ending the `dom` borrow, *then* calls the hook. Nothing on the dialog path
  flushes layout.
- **The slot is released before the call.** `run_dialog` clones the `Rc` out of
  `LoopHooks::dialog_handler` and drops the `Ref` before invoking it, so a
  handler that reinstalls itself is legal rather than a `BorrowMutError`
  (`crates/page/tests/dialogs.rs::a_handler_may_reinstall_itself`).

The handler is installed two ways into one slot. `PageOptions::dialog_handler`
is **required**, not a convenience: `load_html_page` runs inline scripts
*during* the call, so a post-construction setter cannot answer a parse-time
dialog. `Page::set_dialog_handler` mirrors `Page::set_viewport` for a page
already alive.

### One accepted micro-divergence: `alert(undefined)`

The three members go in `crates/idl/webidl/html.webidl` and the code generator
handles them with no change to `crates/idl/src/lib.rs`: `Window` is already in
`this_unwrap`, `cx.this_window` already substitutes the global for a
null/undefined receiver (so bare `alert("x")` works), and
`optional DOMString x = ""` plus `undefined`/`boolean`/`DOMString?` returns are
all existing paths.

The spec declares *two* `alert` overloads, which the generator does not support,
so the optional-argument form stands in. `arg_dom_string_or` defaults only on
`JsValue::Undefined`, so:

| expression | Chrome | OxidePage |
| --- | --- | --- |
| `alert()` | `""` | `""` |
| `alert(null)` | `"null"` | `"null"` |
| `alert(undefined)` | `"undefined"` | **`""`** |

One expression, visible only to an `idlharness`-style test. Buying it back means
either overload support in the generator for one method, or a hand-installed
function that bypasses the IDL path's brand check, `length`, property attributes
and drift protection. Documented instead, and pinned by
`crates/page/tests/dialogs.rs::alert_undefined_shows_the_default_not_the_word`
so it cannot drift silently.

### Console argument values are eager, owned snapshots

The obvious implementation retains the `JsValue`s and renders them when the
embedder drains the stream. It cannot work here. A `JsObject` "must be dropped
before its realm is torn down" (`crates/js/src/value.rs`); the console stream
deliberately survives navigation; and a retained DOM wrapper would pin a node of
a document that no longer exists, deadlocking the wrapper pin bookkeeping. So
every argument is encoded **at `console_write` time** into an owned tree,
`ValuePreview` (`crates/bindings/src/preview.rs`), bounded at depth 4, 100
entries per level, 8 KiB per string and — the load-bearing one — **2048 nodes
in total**, with cycles detected by comparing against the objects on the
current path with `strict_equals`.

The per-level caps alone are not a bound: depth 4 × 100 entries is 10^8
property reads, and path-based cycle detection correctly does *not* reject a
shared sibling, so a shallow graph whose nodes point at each other is re-walked
exponentially. Nothing outside rescues that either — the `ScriptBudget` is
enforced through the engine's interrupt callback, which plain data-property
reads never reach. A container stops filling when the budget runs out rather
than padding itself with `Elided`, so the retained tree is bounded too, not
only the work that built it
(`crates/bindings/tests/console.rs::a_shared_shallow_graph_cannot_blow_up`).

`ValuePreview::Object` also carries a `description`: `Date`, `RegExp` and `URL`
keep their content in internal slots, so an enumerable-property walk sees
nothing at all and they would otherwise preview as `Date {}` — strictly *less*
than the string rendering the encoder replaced. The description is the value's
own `toString` when it is not `Object.prototype`'s uninformative `[object X]`.
`Map` and `Set` fall on the other side of that line and stay `Map {}`: their
contents need iteration, which is out of scope here.

Eagerness also fixes a correctness problem nobody had noticed: script routinely
mutates an object on the line after logging it, and a lazy handle would show the
*later* state (`crates/bindings/tests/console.rs::node_previews_are_snapshots`).

Consequently `ValuePreview::Node` carries a **description string, never a
`NodeId`** — an id in a stream the embedder polls has no re-validation boundary
to be checked at. The generation check happens at encode time, inside the task
that logged it. Stage 6's `RemoteObject` handle table is where node identity
belongs.

The encoder lives in `bindings`, not `js`, because naming a DOM wrapper
(`<div id="app" class="hero">`) instead of walking a whole document needs the
`TAG_NODE` payload and the tree. It reads the tag **from the payload already on
the wrapper** and describes the node from `DomTree` — it never mints a wrapper,
which would pin.

Reading object properties invokes getters and proxy traps, i.e. page code —
exactly as the old `coerce_string` path did. It runs under the armed
`ScriptBudget`, and a throw is contained as `ValuePreview::Threw`, because
`console.log` must not fail.

### Three new engine primitives, each load-bearing

`JsScope` had no property enumeration, no way to distinguish object subtypes,
and no way to capture a stack without throwing.

- **`value_kind` → `ValueKind`.** `JsValue::Object` covers objects, functions,
  symbols, BigInts, arrays, errors and promises indistinguishably, and the
  JS-side tests that could tell them apart are all patchable by page script
  (`Array.isArray`, `instanceof Error`, `Symbol.toStringTag`). rquickjs's
  `Value::type_of()` reads the engine's own tag and is unfakeable. It also fixes
  the reason the old console printed `<unprintable>`: `coerce_string` **throws**
  on a symbol.
- **`own_enumerable_keys(obj, limit)`.** The trait could enumerate arrays but
  not objects — a real asymmetry. `Object::own_keys(Filter::default())` is
  own + enumerable + string-keyed, i.e. exactly `Object.keys`. It takes a limit
  and returns `(kept, total)` because otherwise the breadth cap would not bound
  the *allocation*: an object with five million keys would cost five million
  `String`s for a preview that keeps a hundred.
- **`capture_stack` / `capture_location`.** `JS_NewError` builds a backtrace
  unconditionally, so `Exception::from_message(ctx, "")` + `.stack()` reads the
  current stack without throwing and without disturbing a pending exception —
  which is what a `console.log` that did *not* throw needs for its source
  location. `capture_location` parses only the frame that is kept, since every
  console call takes that path.

**Deviation from the implementation plan:** a fourth primitive,
`symbol_description`, was added. The plan's `ValuePreview::Symbol(String)`
requires the description, and `ToString` on a symbol throws, so there was no
other way to obtain it. It is three lines over rquickjs's `Symbol::description`.

All four use only safe rquickjs APIs. `unsafe` stays denied in `crates/js`.

### `JsError` splits; `ScriptError` is the drained projection

`crates/js/src/error.rs` gains `StackFrame` and `parse_stack`, and
`JsError::Exception` becomes `{ name, message, stack, value }`. The QuickJS-NG
backtrace grammar (`    at <fn> (<url>:<line>:<col>)`, with `(native)` for host
frames) is a `js`-crate concern because it is the engine's format; a V8 backend
would parse the same shape into the same type. Native frames are dropped: they
carry nothing an embedder can act on and are not where the page went wrong.

A thrown *non-`Error`* is rendered by string coercion, with one exception that
had to be special-cased: `ToString` on a symbol **throws**, and the resulting
`TypeError` would be left pending on the context for whatever inspected it next
to pick up and blame on unrelated script. `throw Symbol('boom')` is therefore
named directly (`Symbol(boom)`), and any other coercion failure clears what it
raised.

`Display` becomes the **bare message** — the stack is data beside it now, not
glued on. `JsError::rendered()` reproduces the old two-part form, and is needed
as an identity: `LoopHooks::pending_rejections` retracts an unhandled rejection
by matching it, because the engine's rejection tracker carries no promise
identity. Keying on the bare message alone would retract more aggressively than
the engine rejected, so the key is deliberately message **and** stack — exactly
today's discriminating power.

`JobsOutcome.errors` becomes `Vec<JsError>` and `set_rejection_tracker` takes
`Fn(JsError, bool)`; both flattened to `String` *inside* the `js` crate before,
throwing away the structure this ADR exists to keep.

`ScriptError` (`crates/bindings/src/console.rs`) is the plain-data projection
the embedder receives, since `JsError::Exception` carries the thrown `JsValue`
and must not enter a drained stream. Its `location()` is `stack.first()` rather
than a field: two representations of one fact eventually disagree.

**Deviation from the plan:** `ScriptErrorKind` has a fifth variant, `Resource`.
The plan named four (`Uncaught`, `Callback`, `UnhandledRejection`,
`ScriptBudget`), none of which describes the ~15 event-loop sites that report a
stylesheet 404, an unresolvable module specifier or a rejected web font. Filing
those under `Uncaught` would make `kind` untrustworthy, and `kind` is the whole
point of the field. A driver routes them to `Log.entryAdded`, not
`Runtime.exceptionThrown`.

### One console message type, with the whole payload

`HostHooks::console_message` now takes a built `ConsoleMessage` rather than five
parameters, because the call site is the only place with a scope to read
argument values and capture a stack from. It gains `args` (one preview per
argument, *before* formatting — CDP sends raw arguments and lets the client
format, and so do we), `location`, `group_depth` and `timestamp`.
`ConsoleMessage::engine` is the constructor for a line the engine emitted
itself (`BindCx::warn`, the event loop), which honestly has no arguments and no
call site.

Rendering improves as a by-product: `[1, 2, 3]` and `{a: 1}` instead of
`[object Object]`.

**Format specifiers** (`%s %d %i %f %o %O %c %%`) run the console spec's
Formatter when `args[0]` is a string containing one **and there is more than
one argument** — the Logger's "if rest is empty, perform Printer(logLevel,
« first »)", so a lone `console.log("100%% sure")` keeps its two percent signs.
Leftover arguments are
appended space-separated, a specifier with nothing left to substitute stays
verbatim, and `args` keeps every raw preview regardless. `%c` consumes its
argument and emits nothing — there is no styling in a headless console, and
leaving the CSS in the line would be worse than dropping it.

**New methods**: `trace` (its own level, carrying the stack every message now
captures), `assert` (silent when truthy; otherwise the spec's `"Assertion
failed"` / `"Assertion failed: …"` at `Error` level), `dir` (one argument, no
format pass, no string shortcut), and `group`/`groupCollapsed`/`groupEnd`
maintaining a depth counter on `PageState` that rides on every message. Depth is
the only way grouping is observable headless, which is why it is carried at all;
collapsing is a devtools affordance, so the two group methods are one method.

### Every stream is bounded, and none is cleared by navigation

`console`, `errors`, the pending-rejection buffer and `dialogs` are capped at
1024/1024/1024/256 with front-drop, mirroring `MAX_NAVIGATION_EVENTS` — which
now shares the same `push_bounded` helper instead of open-coding it. Draining
is the embedder's job and nothing forces it, and a console message now retains
an owned tree of previews, so the bound matters more than it did for navigation
milestones. All four are `VecDeque`s: at capacity every push evicts one entry,
and `Vec::drain(..1)` would memmove the whole retained buffer each time — O(cap)
per console line, on the path a chatty page hits hardest.

Bounding the pending-rejection buffer costs the ability to retract its oldest
entry, which is the same trade every other stream makes.

None of the three is cleared on navigation, deliberately: **a navigation must
not erase the errors that caused it.** Console output already behaved this way;
the errors and dialogs follow it, and the dialog *handler* survives too
(`crates/page/tests/console.rs::the_streams_survive_a_navigation`,
`crates/page/tests/dialogs.rs::the_handler_and_the_stream_survive_a_navigation`).

### One home for the payload types

`ConsoleMessage` lived in `page` while `ConsoleLevel` lived in `bindings`, and
the CLI imported from both. All the payload types now live in `bindings`
(`console.rs`, `dialog.rs`, `preview.rs`) — the crate that builds them — and
`oxidepage_page` re-exports them wholesale, so an embedder has one import path
and no `oxidepage_bindings` line in its `Cargo.toml`.
`oxidepage_page::ConsoleMessage` keeps resolving.

## Consequences

**Six breaking public API changes**, all mechanical at the call site:

| change | ripple |
| --- | --- |
| `Page::drain_errors() -> Vec<ScriptError>` | 3 real edits across 38 call sites; the rest are `.is_empty()` or `{:?}` |
| `ConsoleMessage` gains 4 fields, moves crate, loses `Eq` (it holds an `f64`) | the CLI and 2 assertions |
| `HostHooks` gains `run_dialog`, changes `console_message` and `report_error` | the two test harnesses |
| `JsError::Exception` gains `name`/`stack`; `Display` drops the stack | 1 destructuring pattern |
| `JobsOutcome.errors: Vec<JsError>`; `set_rejection_tracker(Fn(JsError, bool))` | 3 sites |
| `PageOptions` gains `dialog_handler` | none (it has a `Default`) |

The signature change also paid for a DRY win it forced: ~11 open-coded
`hooks.report_error(error.to_string())` sites in `crates/bindings/src/lib.rs`
(rAF, observers, MediaQueryList, XHR, custom elements) now go through
`BindCx::report_callback_error`, so they all report `ScriptErrorKind::Callback`
with real frames instead of a bare string.

**What improved beyond the plan's scope**, because the structure made it free:
an error now carries its `name`, which the old rendering dropped; the script-
budget abort names the function that looped instead of reporting an opaque
`InternalError`; and `console.log` of an object shows the object.

**Verification.** `crates/page/tests/dialogs.rs` (12 tests: the default policy,
each response variant, the request payload, the event stream, a parse-time
dialog through `PageOptions`, dialogs from a timer and a listener, handler
reinstallation, the pause property, the `alert(undefined)` divergence, the
stream bound, survival across navigation). `crates/bindings/tests/console.rs`
(23 tests: every preview variant, the depth/breadth/string/node caps, cycles
through objects and arrays, a throwing getter, exotic built-ins keeping their
string form, structural rendering, the Formatter, the new methods, group depth,
locations). `crates/page/tests/console.rs` (9 tests:
locations against a loopback-served script, error kinds, resource errors, the
budget abort's frames and cleared name, all three stream bounds, survival across
navigation).
`crates/js/tests/quickjs.rs` (stack parsing including a native frame, structured
exceptions, `value_kind`, `own_enumerable_keys`, `symbol_description`,
`capture_stack`/`capture_location` from a host callback, a thrown symbol, and
the context left undisturbed by both),
plus unit tests for `parse_stack` and the number/Formatter rules.

WPT is unchanged at 16314/21325 with no expectation drift. **It cannot verify
this stage**: WPT's dialog and error-reporting tests live under
`html/browsers/the-window-object/` and `html/webappapis/`, which are not
vendored under `tests/wpt/vendor/` and not in the runner's `RUN_DIRS`
(`xtask/src/wpt.rs`); vendoring them needs network, which CI never has.
Coverage is the Rust integration suites above. Recorded here the way ADR-0022
recorded the same gap for `html/browsers/history/`.

## Deliberate limits (P6 — absent beats fake)

- **`beforeunload` dialogs, `window.print`, HTTP auth dialogs** — the roadmap's
  own non-goals. Auth arrives with `Fetch.authRequired` in stage 8, and there is
  no unload path to run a `beforeunload` from.
- **No `ErrorEvent` / `window.onerror` / `PromiseRejectionEvent` /
  `unhandledrejection` dispatch.** `"error"` and `"unhandledrejection"` are
  already registered handler types and are settable, but nothing fires them;
  errors reach the *embedder*, not page script. This is a real P6 hole, and it
  is deferred rather than hidden because it is two new IDL interfaces plus a
  dispatch-semantics change — including HTML's inversion where `window.onerror`
  cancels by returning **`true`**, which `crates/bindings/src/events.rs` records
  as unimplemented. `ScriptError` already carries every field `ErrorEvent`
  needs; only the dispatch is missing.
- **Inline `<script>` line numbers are script-relative, not document-relative.**
  `Page::eval_classic` passes the document URL as the filename and QuickJS
  starts counting at 1 for each eval, so an inline script's frames report lines
  within the script text. Browsers report document-relative lines. Fixing it
  needs a line-offset parameter threaded through `JsScope::eval` and a position
  from the parser.
- **`alert(undefined)` shows `""`, not `"undefined"`** — the table above.
- **No `console.count` / `time` / `timeEnd` / `table`**, and `%c` styling is
  parsed and discarded. A `console.group` that did not indent would be exactly
  the always-installed no-op P6 forbids; the ones that shipped are complete.
- **Previews are bounded** (depth 4, 100 entries, 8 KiB strings, 2048 nodes). A
  deep or wide object reports `Elided` / `truncated: true` rather than lying by
  omission.
- **`Map` and `Set` preview as `Map {}` / `Set {}`.** Their contents live in
  internal slots reachable only by iteration, which would run page-visible
  iterator protocol during a log call. `Date`, `RegExp` and `URL` are covered
  by the `description` above because their `toString` is meaningful.
- **`console.group` depth is uncapped** in the payload (opening a group with no
  label costs the page nothing), so a *renderer* must clamp its indentation.
  The CLI does, at 16 levels.
- **Previews run page code.** A getter or proxy trap executes during encoding,
  under the script budget; a throw becomes `ValuePreview::Threw`. A page that
  sets `Error.stackTraceLimit = 0` or overrides `Error.prepareStackTrace`
  degrades every captured stack to empty.
- **Streams are bounded and front-dropped.** An embedder that never drains sees
  the newest 1024 messages, not all of them.
