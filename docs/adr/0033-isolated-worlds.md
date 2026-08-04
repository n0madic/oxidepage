# ADR-0033: Isolated worlds

- Status: accepted
- Date: 2026-08-03
- Builds on: ADR-0011 (observer delivery outside "update the rendering"), ADR-0017 (many documents, one rendered), ADR-0018 (connected-wrapper retention), ADR-0022 (navigation), ADR-0027 (browser, contexts, commands), ADR-0030 (CDP transport and the remote object model), ADR-0031 (the `Input` and `DOM` domains)
- Supersedes: ADR-0030 D8 ("one world, named twice"), ADR-0031 D3 (`DOM.resolveNode`'s `executionContextId` validated then ignored)
- Constrained by: design §2 (P5 backend-neutral display list, P6 "absent beats fake", P7 "conformance is automated"), §5.3 (wrapper cache identity and the pin contract), §7 (threading), §8 (JS containment)

## Context

`docs/automation-roadmap.md` calls stage 9 "the gate to Playwright", and it is
not an exaggeration: Playwright runs *all* of its injected script in a utility
world created with `Page.createIsolatedWorld`, and `addInitScript` and
`exposeBinding` ride the same mechanism. Nothing about Playwright works without
it.

ADR-0030 D8 shipped a stand-in, and named it the single largest divergence in
that stage. A named world is accepted, reports a distinct context id
(`base + ISOLATED_WORLD_ID_OFFSET + index`) so a driver's context map does not
collide with itself, and then acts on the **main** world. The script genuinely
runs, at genuinely the right time; the only property not delivered is the one
the feature is named after. A driver's injected helpers are visible to page
script, and a page that redefines `Array.prototype.map` or `JSON.stringify` can
perturb them.

This ADR replaces that compromise with N real execution contexts per page —
each with its own global, prototypes, wrapper cache, listener registry and
remote-object table — over one shared DOM. It modifies design §5.3, which is
why it exists.

## Decision

### D1 — A world is a whole `Runtime`, not a second `Context` on a shared one

The roadmap prescribed "a second `JSContext` on the *same* `Runtime`", on the
reasoning that contexts of one runtime can share objects like same-origin
frames. **We deviate.** Two properties of `rquickjs-core-0.12.0`, built as we
build it (`features = ["loader"]`, **no `parallel`**), make a shared runtime the
more expensive option rather than the cheaper one:

- **Nested entry is a panic.** `Context::with` takes `self.0.rt().inner.lock()`
  (`src/context/base.rs:118`), and without `parallel` `Mut::lock` is
  `RefCell::borrow_mut()` (`src/safe_ref.rs:1,32-36`). On a shared runtime,
  entering world B from inside a scope on world A is a `BorrowMutError`. That
  nesting is not an edge case — it is exactly what synchronous cross-world event
  delivery *is*: page script calls `el.dispatchEvent`, and a utility-world
  listener on the same node has to run before the call returns. The only escape
  is `pub unsafe fn Ctx::from_raw` (`src/context/ctx.rs:442`), i.e. hand-built
  reentrancy in the middle of the event system, in a workspace where `unsafe` is
  denied everywhere but one module.
- **The leak barrier would have to be hand-built.** `Persistent::restore`
  compares the **runtime** pointer and nothing else
  (`src/persistent.rs:102-110`). On a shared runtime a world-A value restores
  silently into world B — so the failure mode the roadmap names as this stage's
  whole risk ("a wrapper from one world leaking into another") would be caught
  by nothing, and every cross-world seam would need its own audited check.

One runtime per world makes nested entry legal (distinct `RefCell`s), needs no
new `unsafe`, and **gives the leak barrier for free**: a foreign value fails
`restore` with `Error::UnrelatedRuntime`, and because `host_payload` does
`export_obj(o).ok()?` (`crates/js/src/quickjs.rs:632`) a foreign-world host
object yields `None`, so every existing brand check throws the existing
`TypeError: receiver is not a Node`. Verified, non-panicking, and asserted
positively in `crates/page/tests/worlds.rs` rather than assumed.

It also deletes a bug rather than adding one. `QuickScope::pump_jobs` catches
exceptions against `self.ctx` although `execute_pending_job` may have run
*another* context's job — wrong only under a shared runtime. With one runtime
per world, a world's queue holds only its own jobs and the attribution is
correct by construction.

The accepted costs are real and are listed under *Deliberate limits*: one extra
`JSRuntime` per world, per-world resource caps, and per-world microtask queues.

### D2 — A world's native-stack budget is re-anchored where it is entered

This is the one change D1 forces in `crates/js`, and it is a latent bug fix
independent of worlds.

QuickJS records a runtime's stack ceiling **once**, in `JS_NewRuntime`
(`quickjs.c:2019`); the check is `sp < rt->stack_limit` where
`stack_limit = stack_top - stack_size`. rquickjs's `RawRuntime::update_stack_top`
— which `Context::with` dutifully calls — has its body under
`#[cfg(feature = "parallel")]` (`src/runtime/raw.rs:194-199`), so for us it
compiles to nothing. A realm therefore measures its budget from wherever it
happened to be *created*, giving one created deep in the page thread's stack an
effective budget of `max_stack_size + (creation_depth − entry_depth)`:
unbounded, and anchored to the wrong frame.

Measured, not reasoned: a realm created 512 KiB deeper than another ran **1.53x**
as many recursion frames when both were entered from the same shallow point.
With one runtime per world this stops being academic, because a world is
routinely created deep — inside an embedder job, or from another world's host
callback — and `JS_DEFAULT_STACK_SIZE` is 1 MiB against Rust's default 2 MiB
thread stack, so two nested worlds would overflow the page thread outright.

`QuickJsRealm::anchor_stack` calls `JS_UpdateStackTop` at the top of
`with_scope` and `pump_jobs`. Each world's budget is then exactly
`max_stack_size` from its own entry point, which is what makes N nested worlds
bounded against one thread stack. Nothing is restored on exit: entering a
runtime already on the stack is refused a layer up (D4), so every entry is the
outermost one for its own runtime. `RealmOptions.max_stack_size` stops being
`None` at every call site, so the number is ours rather than QuickJS's.

Pinned by `a_realm_created_deep_in_the_stack_runs_with_its_own_budget`.

### D3 — `PageState` splits into `PageShared` + `WorldState`, and `PageShared` holds no `JsValue`

`WorldState` is what `realm.set_state` installs and what `BindCx.state` points
at. It keeps `Rc`-clones of `dom`, `style`, `layout`, `hooks`, `navigator` and
`screen` — `state.dom` alone is 282 sites in `crates/bindings/src`, `hooks` 20,
`layout` 18, `style` 14 — so ~334 call sites need **zero** edits. It owns every
per-realm field verbatim: the wrapper cache, `interfaces`, `slab`,
`same_object`, the listener and handler registries, `observers`,
`connected_wrappers`, `custom_wrappers`, the `*_js` singletons, the observer
and resolver tables, `pending_net`, `adopted_sheets`, the remote-object store.
The observer **delivery gate** (`obs_gate`/`obs_dirty`) belongs here too, next
to the registries it gates: `deliver_observations` fans out over the worlds, so
a page-level gate would let the main world's pass re-stamp it and every world
after it short-circuit — an isolated world's `ResizeObserver` and
`IntersectionObserver` callbacks would never fire at all.

`PageShared` takes the bare page-level cells (`parsing`, `ready_state`, the
parser-write pair, `pending_navigation`, `history`, `timing`, …)
plus the registries that must be world-aware: `worlds`, `next_context_id`,
`init_scripts`, `bindings`, `binding_calls`, `net_world`, and the connectivity
log of D7.

The invariant that makes teardown tractable is that **`PageShared` holds no
`JsValue`**. The one violator was `SessionHistory::HistoryEntry.state`. It
becomes a serialized string rather than a `(WorldId, JsValue)` pair: the spec
structured-clones the state object anyway, so a per-world copy built by that
world's own `JSON.parse` is *more* conformant than a shared handle, and it is
the difference between `history.state` working in every world and reading
`null` in all but one — which would break Playwright's navigation waits.

The ~15 existing helper methods (`epoch_now_ms`, `request_navigation`,
`mark_timing`, `queue_parser_write`, `take_pending_navigation`, …) stay as
one-line delegates on `WorldState`, so `cx.state.request_navigation(..)` reads
identically. Net cost: ~60 mechanical `cx.state.X` → `cx.state.page.X` edits
plus the struct move.

The rejected alternative was to keep one `PageState` and key every per-world map
by `(WorldId, key)`. That changes the key type of ~40 maps, forces a world field
into `EventTargetKey` and therefore into every listener, handler and
finalization path, and demotes "one wrapper per node per world" from a
type-level fact to a convention. The `Rc`-clone split costs its 60 edits once.

### D4 — Drop order is the sharpest hazard, so it is explicit rather than encoded

A `Persistent` outliving its runtime aborts the process in `JS_FreeRuntime`
(ADR-0030 D3). With N runtimes the ordering is spread across `Page`,
`WorldTable` and `World`, and a page-level container can hold values from
several worlds at once.

Four things hold the line. `PageShared` holds no `JsValue` (D3). `Page`'s field
order is drop order, and `impl Drop for Page` releases every page-level JS value
— timer and rAF callbacks, the only page-level JS `LoopHooks` still owns —
before any field drops. `WorldTable::teardown` destroys each world completely
before touching the next, clearing the world's state before its realm, so a
future field reorder cannot silently reintroduce the abort. And the teardown
`debug_assert`s `Rc::strong_count(state) <= 2`, which is the mechanical check
that "`PageState` is recovered from the JS scope, never captured" still held.

There is no `Rc` cycle: `WorldTable` owns realms, a realm owns its
`Rc<WorldState>`, a `WorldState` owns its `Rc<PageShared>`, and `PageShared`
holds only a **`Weak<dyn WorldEnter>`** back to the table. That `Weak` is how a
host callback in world A reaches world B, since `native_inner` rebuilds `BindCx`
from `scope.state()` alone.

Entering a world already on the stack is refused by a per-world latch and
reported, rather than left to `RefCell` to turn into a panic. The reachable
shape is ordinary script: A dispatches → B's listener runs → B's listener
dispatches → delivery back into A while A is on the stack. Refusing the
re-entrant delivery is the sharpest behavioural cost of D1 and is listed as a
limit.

Because the failure mode is a process abort rather than a test failure,
`dropping_a_page_with_live_worlds_is_clean` and a panic-mid-dispatch case exist
specifically for it.

### D5 — A value created in world A reads as `null` in world B

Cross-world values are impossible by construction (D1), so every `JsValue`
reachable from more than one world needs an explicit rule. We take Blink's
`WorldSafeV8Reference` rule: the value is tagged with its world and reads as
`null` from any other. The tag is checked before `restore` is called, because an
unchecked read surfaces as an opaque engine error blamed on unrelated script.

`EventData.detail` currently backs three members through one `JsValue`; it
becomes

```rust
enum EventDetail { None, Node(PinnedNode), Value { world: WorldId, value: JsValue } }
```

- `SubmitEvent.submitter` → `Node`. It is stored as a *wrapper* today purely to
  pin the node; keeping the pin and dropping the wrapper removes the leak **and**
  the stale-id hazard the existing comment describes.
- `CustomEvent.detail` → `Value`, readable only in its own world. This is the
  isolation boundary doing its job, not a gap, and it is what Chrome does.
- `PopStateEvent.state` → built **per world** by `fire_pop_state` from the
  serialized history entry (D3), so every world sees its own.
- `UiPayload`'s related target (`MouseFields.related`, `UiKind::Focus`) →
  `PinnedNode`, resolved per world. `e.relatedTarget === node` still holds
  *inside* a world, because `node_to_js` is a cache lookup.
- `LoopHooks::Timer.callback` and `raf_callbacks` gain a `WorldId` and the loop
  enters that world to run them. Ids stay page-global, so `clearTimeout` and
  `cancelAnimationFrame` need no world.
- A foreign-world `AbortSignal` passed to `addEventListener` is a `TypeError`
  rather than a silent ignore. This needed **no code**: `abort_signal_key`
  reads a slab key out of the value's host payload, and `host_payload` cannot
  read a foreign object's payload at all (D1), so the existing brand check
  already throws. `JsListener.signal` therefore stays a plain `u64` — the leak
  the plan worried about is structurally impossible, not merely handled.

### D6 — One event, N wrappers; main world first, then creation order

`HostData::Event(Rc<RefCell<EventData>>)` already shares state through an `Rc`,
and `target` / `current_target` / `path` are already `EventTargetKey`, so
per-world resolution is nearly free. `EventData` gains
`iface: &'static str` (written by `new_event_object`, whose `interface`
parameter tightens to `&'static str`) so any world can mint the right
subinterface.

`dispatch_event` drops its `event_value: &JsValue` parameter and takes the
`Rc<RefCell<EventData>>` plus a stack-local per-world wrapper map, materialised
**lazily**: a world gets a wrapper only when it actually has a listener on the
current path node, so a page with no utility-world listeners pays one hash probe
per node per world and never enters a second scope. The hop must not straddle a
`RefCell` borrow on dom/style/layout ("reflow must never re-enter JS");
`dispatch_event` already builds and clones the path before invoking listeners,
and a `debug_assert` now says so.

Listener registries are **per world**, which keeps each world's `JsValue`s
inside it and keeps `process_finalized`'s `TAG_SLAB` purge world-local — world
A's slab key 7 can only ever reach world A's slab, listeners and handlers.
`EventTargetKey::Window` then unambiguously means "this world's window" and
needs no change.

**Ordering rule, and it is a documented divergence:** within a world,
registration order (unchanged); across worlds, main world first then creation
order. Registration order *within* a world is the only order the spec and Chrome
guarantee, and Blink keys its listener map by world too, so cross-world order is
unspecified. Main-first is the useful choice: it lets a utility-world listener
observe the page's own `defaultPrevented`, which is exactly what a driver
reading the outcome of synthesized input wants.

`stopPropagation`, `stopImmediatePropagation`, `preventDefault` and `canceled`
live in the one shared `EventData`, so one event has one propagation no matter
which world cancels it — the spec behaviour, and free. Activation behaviour runs
in the main world always: a `click` that submits a form must not behave
differently depending on which world dispatched it.

`handlers::resolve` skips the content-attribute half in a non-main world. An
inline `onclick=` is page script and compiles once, in the main world; Chrome
does the same.

### D7 — Connected-wrapper retention becomes a log with per-world cursors

`DomTree::take_pinned_connectivity` is `std::mem::take` — **destructive**. With
N worlds the first to drain starves every other, and ADR-0018's expando
guarantee silently breaks for the rest. It is replaced by
`PageShared::connectivity`, a sequence-numbered log, plus a per-world cursor:
the drain moves the DOM's pending connectivity into the log, consumes from
*this* world's cursor (revalidating each `NodeId` at the boundary, as today),
and trims below the minimum live cursor.

The active world still drains synchronously in `native_inner`, so ADR-0018's
guarantee is preserved for the world whose script is running; other worlds drain
at the loop's connectivity step and on their next entry. The residual window —
world A connects a node while world B holds an unretained wrapper, and A's
allocation triggers a GC before B next runs — is closed by **provisional
retention**: `node_to_js` retains a wrapper minted for a *disconnected* node in
`WorldState::pending_conn`, and the next drain promotes it into
`connected_wrappers` or drops it. Bounded by "detached nodes this world wrapped
since its last drain".

This also fixes a latent **single-world** bug: a node wrapped while detached and
then connected by the *parser* (not by a host call) can today lose its expandos
to a GC before the deferred drain.

`same_object` is per world, and when `dom.get(id).is_none()` — every world's pin
released — `process_finalized` purges **every** world's cache, so no world keeps
a dangling entry until its own wrapper happens to finalize. `DomTree::pin` is
already a refcount (`pins: HashMap<NodeId, u32>`), so N worlds → N pins works
with **no change to `crates/dom`'s pin API**.

### D8 — `customElements` is absent in an isolated world (P6)

`install_custom_elements` is skipped when the world is not the main one, so
`window.customElements` is simply **not installed**. `custom_elements`,
`custom_wrappers` and `construction_stack` stay main-world.

The justification is stronger under D1 than it would have been under a shared
runtime: a definition's constructor is a *main-world function*, so a non-main
registry could not invoke it at all — `restore` would refuse. A
present-and-throwing `customElements` would therefore be a fake in the P6 sense,
an always-failing stub that defeats feature detection, about a capability that is
structurally out of reach rather than merely unimplemented. Absent is
detectable, and it is precisely the roadmap's "the utility world sees upgraded
elements but cannot define new ones". Chrome *does* give isolated worlds a real
registry; that is recorded below as a divergence and a non-goal.

Two `native_inner` hooks gate on the main world for the same reason and defer to
the event loop's own steps, which run there: `invoke_custom_element_reactions`
(reactions stay on the DOM's backup queue) and `run_pending_inline_scripts` (a
`<script>` inserted from the utility world stays in `dom.script_updates()` and
runs as page script, which is what it is).

`microtask_checkpoint`'s own `drain_custom_element_reactions` gates on the same
flag, and that one is **not** decoration. The reaction queue is the shared
DOM's while the definitions are per world, so a checkpoint taken in a world with
no registry pops the page's `connectedCallback`/`attributeChangedCallback`
intents and finds nothing to run them with — silently dropping them. Since the
loop now pumps every world's job queue as a task source, one settling promise in
a driver's utility world was enough to swallow the page's upgrades.

### D9 — A world is identified by its name, is idempotent, and is rebuilt at every commit

CDP offers no handle for a world but its name, so the name is the identity
(`""` for main). `Page.createIsolatedWorld` is **idempotent by name** within a
document: it returns the same context id and re-emits
`executionContextCreated`. Chrome mints a fresh context per call, but drivers
call it once per navigation and the protocol has no way to destroy the surplus,
so minting would leak a context per navigation for the lifetime of the page.

**On commit every isolated world is torn down and rebuilt** under the same name,
with a fresh global and a **new** context id. This is not an optimisation to
skip: a `worldName` init script must run against a fresh global, and the world's
wrapper cache, slab, listeners and object store all name the dead document.
`reset_worlds_for_navigation()` runs before `run_init_scripts` and re-applies
`PageShared::bindings`.

**`Runtime.executionContextDestroyed` is not emitted.** A commit already emits
`executionContextsCleared`, which is what both drivers act on, and stage 9 has
no single-world teardown path outside a commit. The engine-side hook exists so
the event can land with stage 10's frame detach.

`grantUniveralAccess` stays accepted-and-ignored: there is one origin per page
until stage 11, so there is nothing to grant across.

### D10 — Context ids are page-level and monotonic; `WorldId` does not cross the thread boundary

`ISOLATED_WORLD_ID_OFFSET` and `world_context_id` are **deleted**. Context ids
come from one page-level monotonic counter, unique across documents *and*
worlds, which is what the offset was faking.

The world registry **moves from `SessionState` to the page**:
`SessionState::isolated_worlds` and `isolated_world_index` are deleted. Two
sessions on one target must see the same worlds — today session 1's
`createIsolatedWorld` is invisible to session 2, and to session 2's
`Runtime.evaluate { contextId }`.

Only the `context_id: u64` crosses the thread boundary; the dense `WorldId`
stays inside `page`. A `WorldId` is recycled when a world is rebuilt, so a stale
one would silently hit a live world, where a stale monotonic context id is a
clean "Cannot find context with specified id".

The world table in `page` is the **one** registry: it holds each world's name
and its realm, and every report of a context id reads the live
`WorldState::context_id`. `PageShared::worlds` is a list of `WorldId`s and
nothing more — it exists only as `world_ids`' fallback for an embedder that
installs worlds without a table. It deliberately does **not** mirror the name or
the context id: the main world's realm survives a commit and
`reset_for_navigation` renumbers it on the live `Cell`, so a mirrored copy is
stale from the first navigation onwards, and a lookup answering with the dead id
is worse than no lookup at all.

`objectId`s stay page-unique via the existing monotonic counter; each store
entry records its world, `callFunctionOn` takes the world **from the handle**
(a conflicting `executionContextId` is an error, as in Chrome), and
`releaseObjectGroup` sweeps all worlds. `ConsoleMessage`, `ScriptError` and
`PageEvent::Binding` carry the world's context id, so `pump.rs` stops reporting
`unwrap_or(1)`.

`Page::worlds()` is a **control** job, for the same reason
`execution_context_id` is: the CDP event thread reads it and must never block
behind a navigating page. It reads a `RefCell` and clones names and ids — no JS,
no DOM, no layout — which stretches CLAUDE.md's "`Cell`s and channels only"
convention far enough to be worth naming here rather than leaving to be
rediscovered.

## Consequences

`Page::evaluate`, `add_binding`, `add_init_script` and `node_object` keep
today's signatures as main-world wrappers over the new `*_in` forms, so `cli`,
`crates/page/tests/remote.rs` and the existing CDP tests are untouched by a
change that rewrites the state layer underneath them.

`crates/bindings/src/generated.rs` needs **no change**: every glue function is a
world-neutral `fn` pointer reaching state only through `BindCx`. The codegen was
not re-run.

The event loop keeps its fixed task-source order; eight steps become "for each
live world, main first". `microtask_checkpoint` pumps every world to
quiescence rather than once, because a world-A mutation can queue a world-B
MutationObserver microtask. `js_heap_used` sums across worlds, which is the
honest number a driver watching for a leak needs.

Three documents change with this one. Design §5.3's wrapper cache identity and
pin contract are now per world, and CLAUDE.md's paragraph on them says so.
ADR-0030 D8's compromise is deleted rather than widened, and its "no isolated
worlds" limit is struck. ADR-0031 D3's `executionContextId` stops being
"validated, then ignored" — `DOM.resolveNode` mints the handle in the named
world.

The deviation from the roadmap's stage-9 text is D1, and it is the whole shape
of the stage: "a second `JSContext` on the same `Runtime`" would have needed
hand-built reentrancy through `unsafe { Ctx::from_raw }` and a hand-built leak
barrier, to buy a value-sharing capability that D5 then spends the rest of the
ADR forbidding.

Two defects survived the first implementation and were caught by review, both
reproduced before being fixed. They are worth naming because each is a *rule*
the design states and the code then quietly broke:

- **The main world's re-entry latch was never armed.** `with_cx_in`
  short-circuited `MAIN_WORLD` straight to `with_cx`, which does not go through
  `WorldEnter::enter` — so a delivery arriving back into main while main's own
  scope was live re-entered a borrowed `Context` and panicked, killing the page
  thread. Reachable from ordinary page script: click → utility-world listener →
  `dispatchEvent` back into main. `WorldTable::mark_entered` is now the single
  place the latch is taken, and `with_cx` takes it too.
- **A shared `EventData` held world-owned `JsValue`s.** `wrap_event` files the
  same payload into every world's slab, and a slab is not cleared on navigation,
  so a main-world wrapper kept an isolated world's `CustomEvent.detail` — or a
  UI event's `relatedTarget` — alive past that world's teardown, and freeing the
  runtime aborted the process. `relatedTarget` became `PinnedNode` as this plan
  originally specified and the first implementation skipped; `detail` keeps its
  value but is registered weakly in the owning world, which `release_js` clears.

The general rule both violated: **anything reachable from a shared payload is
reachable from every world, so it must either hold no `JsValue` or be
findable by the world that owns it.**

Four more things this work found that the plan did not predict, each pinned by a
test:

- **`navigator.languages` / `plugins` / `mimeTypes` cached their wrappers on the
  shared `NavigatorData`** — a `JsValue` of whichever world asked first, which
  is both a cross-world leak and a page-level holder of JS values. Now per world.
- **Promise settling read the value in the main world**, so an `await` in a
  utility world reported `undefined` for a promise that had really fulfilled.
- **The event loop pumped only the main world's job queue.** Job queues are per
  runtime, so a promise created in a utility world — where a driver's entire
  injected surface lives — never settled at all. `pump_non_main_jobs` is a task
  source now.
- **`MutationObserver` delivery needed a task source too.** A mutation enqueues
  the compound microtask on the queue of the world that *made* it, so every
  other world's observers were never told.

**Verification.** `crates/page/tests/worlds.rs` (35 tests) covers lifecycle,
isolation, the wrapper/pin contract across worlds, per-world task sources,
cross-world dispatch with its ordering rule, and the drop-order regressions —
it lives at the `page` level rather than in `bindings` because the cross-world
hop goes through the page's world table, and a bare `install_world` embedder has
no table to hop with. `crates/cdp/tests` inverts the one-world tests into their
real form and adds isolation, routing, re-announcement and two-session cases.
`cargo xtask puppeteer` went 45/45 → **48/48**, the new checks being the
isolation itself; `cargo xtask playwright` is new at **13/17** under the same
two-sided expectation contract — a regression, an unexpected pass and a stale
entry all fail CI.

## Deliberate limits (P6 — absent beats fake)

- **`customElements` is not installed in an isolated world** (D8). Chrome gives
  one a real registry. An isolated world still sees upgraded elements as
  ordinary `HTMLElement`s, and a `<script>` it inserts still runs in the main
  world.
- **Microtask ordering is per world.** Within a world ADR-0011's guarantee is
  untouched: the MutationObserver compound microtask still rides that world's
  own job queue, so `await Promise.resolve()` cannot overtake records queued
  before it. *Across* worlds there is no defined order, and there cannot be with
  two job queues — Chrome, with one isolate, does order them. Unobservable
  except through the DOM, because no injected script races the page's
  microtasks.
- **Re-entering a world already on the stack is refused, not queued.** A → B → A
  delivery skips the innermost hop and reports it (D4). Making it work needs the
  shared-runtime `Ctx::from_raw` D1 declined.
- **The JS memory cap and stack limit apply per world**, so a page's ceiling is
  `worlds × memory_limit`. Bounded because page script has no path to create a
  world: the only creation paths are driver commands, and CLAUDE.md already
  records that reaching the endpoint is equivalent to owning the process. A
  world count cap bounds a buggy driver.
- **No `Runtime.executionContextDestroyed`** (D9). `executionContextsCleared` on
  commit is what both drivers act on.
- **`createIsolatedWorld` is idempotent by name**, where Chrome mints a fresh
  context per call (D9).
- **No per-world CSP and no per-world prototype-poisoning protection**, as the
  roadmap's non-goals set out. Isolation is of globals and wrappers; the DOM
  underneath is one shared, equally-trusted tree.
- **`grantUniveralAccess` is accepted and ignored** — one origin per page until
  stage 11.
- **Four Playwright checks fail pending the next stage**, listed in
  `tests/playwright/expectations.tsv`: `page.fill` appends rather than replacing
  (the `Input` domain does not honour a selection), and `page.setContent`,
  `page.waitForSelector` and `page.exposeBinding` need injected-script and frame
  plumbing that is stage 10's scope, not worlds.
- **`Emulation.setEmulatedMedia` accepts each media feature's default** and
  refuses every other value, on ADR-0030 D9's "asking for the state that already
  holds is not a lie" rule — Playwright sends four of them while creating every
  page. The caveat worth stating: `matchMedia` reports `prefers-reduced-motion`
  and `forced-colors` as **not** matching whatever the driver sets, because
  stylo does not implement those features. That gap predates this change; the
  no-op acceptance does not widen it, but a driver that sets one and then
  asserts the query holds will disagree with the page.
