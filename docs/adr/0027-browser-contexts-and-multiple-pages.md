# ADR-0027: Browser, contexts, multiple pages, and async commands

- Status: accepted
- Date: 2026-07-28
- Builds on: ADR-0022 (the navigation task source and its `navigating`/`parsing`
  guard, which the command task source reuses verbatim) and ADR-0025, whose
  bounded drained streams are the payloads the push bus carries.
- Constrained by: ADR-0004 D4 (the page thread's single blocking wait) and
  ADR-0005 D3 (stylo's thread-locals, which are why a `Page` cannot move
  between threads).
- Supersedes: the `Browser`-is-unneeded paragraph of design §5.12 (see Context).

## Context

Stage 5 of `docs/automation-roadmap.md` is design Phase 8, and it is the hard
prerequisite for every protocol stage after it. Today a `Page` is a synchronous
single-threaded loop that an embedder drives by calling `settle(budget)` and
then polling `drain_console` / `drain_errors` / `drain_dialog_events`. A
protocol server needs the opposite shape on both axes: it must deliver commands
and receive answers *while* the page is running, and it must be pushed events as
they happen rather than discovering them at the next poll. It also needs more
than one page, in more than one isolation group.

**The design document contradicts itself here, and this ADR picks the side.**
§5.12 records that the `Browser` indirection "turned out unneeded" and that
"multiple pages are just multiple `Page`s sharing nothing but the process". §7
says the opposite in two bullets: multiple pages are "independent page threads
sharing the net pool, HTTP cache (keyed correctly), and font collection", and
"`Browser`/`Page` public handles are thread-safe façades over command
channels". Both cannot be true. §5.12 was written when `page` was the whole
embedding API and the observation behind it was accurate *for that scope* —
nothing in a page's state needs to cross a thread boundary. What it missed is
that the things worth sharing between pages are not page state at all: a
connection pool, a response cache, a cookie jar, a font scan. Stage 5 implements
§7; **§5.12's parenthesis is superseded by this ADR** and the `engine` crate
stops being a stub.

The acceptance criterion the roadmap names for the loop change is not a
behavior, it is a property: ADR-0004's "one blocking wait, no busy-wait" must
survive. That constraint shapes D3 and D4 more than anything else in this
document.

## Decision

### D1. One thread per page; the handles are `Send` façades over channels

`Page` is `!Send` and stays that way. This is not an accident to be engineered
around: rquickjs is pinned to `=0.12.0` **without** the `parallel` feature, so a
`Runtime` and every value in it are thread-affine; and stylo's style-sharing
cache, bloom filter and the `ACTIVE_TREE` handle deref are thread-locals
(ADR-0005 D3). The DOM arena is `Rc`-shared throughout. A `Page` that could move
threads would be a different engine.

So the concurrency lives one level up. `crates/engine` gains `Browser` →
`BrowserContext` → `PageHandle`, all `Send + Sync`, each a handle over a
`crossbeam_channel::Sender` to an OS thread named `page-{id}` that owns exactly
one `Page` for its whole life. The page never leaves its thread; only messages
cross. This is §7 as written, and it is the only shape the two pins above allow.

### D2. Commands are boxed closures, not a command enum

```rust
pub struct PageJob {
    /// Runs at any wait point, not only at the top of the loop (D3).
    pub control: bool,
    pub run: Box<dyn FnOnce(&Page) + Send>,
}
```

The obvious alternative — an enum of commands with a matching enum of responses —
was rejected for two reasons, one about volume and one about types.

The volume argument: `Page`'s public surface is roughly 35 methods today and
grows with every stage. A command enum duplicates all of it twice (request and
response) and forces a third edit, the `match` arm, for every method added. That
is three places to drift where the WebIDL pipeline taught this codebase to have
one.

The types argument is the decisive one. `Page::dom()` returns
`Ref<'_, DomTree>`; `layout()` and `style()` return the same shape. A borrow
guard is neither `Send` nor `'static` and **physically cannot cross a channel**.
An enum-based protocol would have to enumerate, in advance, every owned
projection any caller might ever want. A closure inverts that: it runs *on the
page thread*, takes the borrow there, and sends back whatever owned `Send` value
the caller actually needs.

`engine` wraps this in typed methods (`navigate`, `eval`, `screenshot`, …)
through a single private `call()` helper that pairs a job with a one-shot reply
channel, so the ergonomics are ordinary. `PageHandle::with(|page| …)` is the
escape hatch, and it covers the entire tail of the `Page` API with no per-method
work — including methods that do not exist yet.

### D3. Commands are a task source, and they obey the navigation guard

Ordinary jobs land in a `pending_jobs` queue and drain inside
`Page::run_until_stalled_until`, immediately after the pending-navigation drain
and under the **same** `!self.navigating.get() && !self.state.parsing()` guard.

Running a job the moment it arrives is not an option, and the reason is
concrete. `Page::await_subresources` and `Page::await_pending_stylesheets` both
call `run_until_stalled_until` from *inside* `load_document_inner`, where the
page is mid-parse: it holds `RefCell` borrows on dom and style and live handles
into the parser. Executing `page.eval()` there is not a race, it is a
deterministic `BorrowMutError` — the same hazard as "reflow must never re-enter
JS", reached from the other direction. The guard that already exists for script-initiated
navigation is exactly the right guard for this, for exactly the same reason, so
it is reused rather than reinvented.

`control` jobs are the exception and are deliberately a small, closed set:
**close, stop-loading, and dialog answer**. Each touches only `Cell`s and
channels — none enters JS, takes a DOM borrow, or flushes layout — so each is
safe to run at any wait point, which is what makes a page interruptible while it
is blocked on a slow document load. Anything that does not meet that bar is an
ordinary job.

### D4. One blocking wait, preserved literally

The three `Receiver::recv_deadline` sites in `crates/page/src/lib.rs` —
`settle`, `await_subresources` and `await_pending_stylesheets` — collapse into
one private helper:

```rust
fn wait_for_work(&self, deadline: Option<Instant>) -> WaitOutcome
```

built on `crossbeam_channel::Select` over the net receiver and the command
receiver. Every caller passes the deadline it already computed. There is still
**exactly one park per loop iteration**, and it still wakes on the earliest of a
net event, the next timer, the next rendering opportunity, or the budget end —
now with "a command arrived" added to the set. ADR-0004 D4's property is
preserved literally, not morally: the count of blocking waits per iteration is
unchanged at one.

`deadline: None` blocks indefinitely, which is new and strictly better. A page
with a command port and nothing to do parks until someone speaks to it, at 0%
CPU. Today the same page has no such state — it returns from `settle` and the
embedder decides when to call again.

**The trap, recorded because it is silent and catastrophic.** A *disconnected*
receiver registered in a `Select` is permanently ready: the receive operation
completes immediately with `RecvError` forever. Re-selecting over a channel
whose last `Sender` has dropped therefore converts the single blocking wait into
a hot spin — the exact busy-wait ADR-0004 exists to prevent, arriving through
the change that was supposed to preserve it, and visible only as 100% CPU on an
idle process. So a disconnected command channel **ends the command loop** rather
than being registered again; the last `PageHandle` dropping is the page's
shutdown signal, which is also the semantics an embedder expects.

### D5. The thread driver lives in `page`; the policy lives in `engine`

`Page::run_command_loop(rx)` is a `page` method. It has to be: only `page` can
own the receiver alongside `net_rx`, and only `page` can reuse `wait_for_work`
and the drain order. Putting the loop in `engine` would mean re-exporting the
event loop's internals, which is how the funnel gets a second entrance.

`page` nonetheless learns nothing about `Browser`, `BrowserContext` or CDP,
because a job is an opaque `FnOnce(&Page)`. The layering in CLAUDE.md holds
unchanged — `page` is still the crate that sees the whole stack, and `engine`
sits above it as a consumer.

### D6. `PageEvent` is a push bus laid over the existing funnels

```rust
Page::set_event_sink(Option<Rc<dyn Fn(PageRecord)>>)
```

There are already exactly four places in `crates/page/src/lib.rs` where an
observable page fact is recorded: `LoopHooks::console_message`,
`LoopHooks::report_error`, `LoopHooks::run_dialog` and
`Page::record_document_milestone`. Each pushes to the sink when one is
installed, in addition to its existing `VecDeque`. No new funnel is created,
which is the point: a second recording path is a second thing to forget.

**Unhandled promise rejections are the one deliberate exception**, and the
reason is a correctness property, not a shortcut. A rejection is only an error
if nothing ever handles it, and a handler may attach much later
(`p = fetch(…)` now, `p.catch(…)` next tick). `LoopHooks::pending_rejections`
therefore holds them and *retracts* one whose handler shows up. Pushing at the
funnel would emit errors the engine subsequently un-rejects. They are instead
flushed at the end of a top-level `run_until_stalled_until` pass — the same
"last moment a handler could have attached" boundary that `Page::drain_errors`
already uses, and the same answer it already gives.

With **no sink installed the behavior is byte-for-byte today's**: one branch on
an `Option`, no allocation, no ordering change. The CLI is untouched and the
pull API (`drain_console` / `drain_errors` / `drain_dialog_events` /
`drain_navigation_events`) stays exactly as it is. Push and pull are two views
of the same four funnels, not two mechanisms.

### D7. A shared `NetPool`, a partitioned cache, a cookie jar per context

§7 requires the net pool, the HTTP cache and the font collection to be shared
across a browser. `NetPool` owns the pieces that are genuinely shareable:

- **the tokio runtime.** One multi-thread runtime for the browser instead of one
  per page (`NetService::new` builds a two-worker runtime *per page* today — at
  ten pages that is twenty threads for a workload that is almost entirely socket
  waiting).
- **`HttpClient`**, which is `Clone` and shares its hyper connection pool
  (`crates/net/src/client.rs:27`). Sharing it is the whole point: two pages
  hitting the same origin reuse a warm TLS connection.
- **`Arc<Mutex<HttpCache>>`**, keyed by `(CachePartition, method, URL)`. The
  partition is what makes sharing safe rather than a leak: a cache shared across
  contexts lets one context probe another's browsing history through hit/miss
  timing. Each `BrowserContext` gets a partition; a standalone `NetService` uses
  `CachePartition::default()` and behaves exactly as before.

`Vary` is untouched by the partitioning. It is resolved *inside*
`CachePolicy::before_request` after the key lookup, so adding a component to the
key cannot disturb it — the two mechanisms operate at different stages.

**The cookie jar is per `BrowserContext`**, which is the isolation users
actually mean by "incognito context": two pages in one context share a login,
two contexts do not.

**Per-page byte and request budgets stay per page.** `ResourcePolicy`'s
`max_total_bytes` and `max_requests` (`crates/net/src/policy.rs:37`) are
enforced against cumulative counters that live on `FetchEngine`
(`crates/net/src/fetch.rs`). Sharing one `FetchEngine` across a browser
would silently reinterpret "500 requests per page" as "500 requests per browser"
— a limit that changes meaning when a second page opens is worse than no limit.
So each page keeps its own `FetchEngine` (its own counters) over the pool's
shared client and cache.

`Runtime::block_on` from several page threads against one runtime is legal, and
for the reason ADR-0004 D4 already recorded: a page thread is never a runtime
worker, so blocking it parks that thread only while tokio's workers keep
delivering bytes to every other page.

**Dropping the pool is safe from anywhere.** Dropping a tokio `Runtime` blocks,
and tokio panics rather than deadlocking if that happens inside an async
context. A single-page `NetService` could rely on dying on the thread that built
it; an `Arc<NetPool>` shared by every page of a browser cannot dictate which
holder drops it last. `NetPool::drop` therefore hands the shutdown to a plain
thread when it finds itself on a runtime, instead of leaving a panic for
whoever happens to hold the final reference. The client/policy pairing is
protected the same way — by construction rather than by a rule: the policy is a
private field of `SharedFetchParts`, and `NetPool::shared_parts` is the only
thing that can mint one, so a client built for a permissive policy cannot be
handed a strict one to re-check redirects against (ADR-0004 D1).

### D8. `ResourcePolicy` is a browser-level decision

`HttpClient::new(policy)` bakes the SSRF connector into the client
(`crates/net/src/client.rs:36-43`), and the connector is the single enforcement
point ADR-0004 D1 is built around. A shared hyper pool therefore implies a
shared policy — a per-context policy would need a client per context, which
throws away the connection reuse that motivated sharing in the first place.

That is the right level anyway. Whether this process may reach `127.0.0.1` is a
deployment and security decision about the process, not a per-tab preference,
and letting a protocol client widen it per context would turn context creation
into a privilege escalation.

### D9. Fonts are already shared; nothing is built

§7's "shared font collection" needs no work, and this is recorded so nobody
implements it twice. `font_context_template` (`crates/layout/src/fonts.rs:172`)
caches two process-wide warm `FontContext` templates behind a `static Mutex`;
cloning one bumps refcounts on the shared fontique `System` and deep-copies only
the collection's small private data, so **the system-font scan happens once per
process** however many pages exist.

The *non*-sharing on top of it is equally deliberate: the collection is built
with `CollectionOptions { shared: false }`, so a page's `@font-face`
registrations write into its own clone and never reach a sibling. That is a
property of fontique rather than a convention, and
`web_fonts_do_not_leak_between_font_systems`
(`crates/layout/src/fonts.rs:427`) pins it. Web fonts leaking between pages
would be a cross-page information leak in a browser and a determinism bug here.

### D10. Suspending freezes the page, not just its command port

`Page::new` builds a page over an empty `about:blank` document and **does not
navigate**; `PageOptions::url` only seeds the document URL. For the case the
engine actually uses — suspended from birth — a suspended page therefore needs
almost nothing: there are no timers, no network and no script to hold back, and
`run_command_loop` simply services `control` jobs until `PageHandle::resume()`
arrives.

It would have been tempting to stop there and call suspension a driver state
with no counterpart in `page`. That is wrong for the *general* case, and the
method is public: a page suspended after it is already running would keep firing
its timers and executing script while refusing every driver command — a page
that runs attacker-controlled code and answers nobody, which is the opposite of
what a driver suspends for. So `Page::suspend` is a real mode: while it is set,
`run_until_stalled_until` returns after GC bookkeeping and runs no task source
at all, and the loop parks indefinitely rather than waking for timers it will
not fire. `Page::settle` returns immediately rather than sleeping out a budget
nothing can consume.

This is where two later stages attach: stage 7's
`Page.addScriptToEvaluateOnNewDocument` (which must run before the first
document's scripts) and stage 10's `Target.setAutoAttach {
waitForDebuggerOnStart: true }` → `Runtime.runIfWaitingForDebugger`, which is a
hard Playwright requirement.

### D11. Dialogs are answered over a dedicated channel, with a mandatory exit

ADR-0025 built `HostHooks::run_dialog` synchronous on purpose and recorded that
the real-CDP variant — where the renderer *blocks* between
`Page.javascriptDialogOpening` and `handleJavaScriptDialog` — becomes possible
at this stage. It becomes possible, and the handler signature does not change:
the `DialogHandler` that `engine` installs blocks waiting for the answer.

**The dialog is announced before the handler runs**, as
`PageRecord::DialogOpening` → `PageEvent::DialogOpening`, with the completed
`PageEvent::Dialog` following once it is answered. That ordering is the whole
usability of the feature and not a nicety: the handler *is* the wait, so a
driver told only afterwards would be waiting on an event that cannot arrive
until the wait it is meant to end has already timed out. Announcing first is
also what CDP does (`Page.javascriptDialogOpening`).

The answer channel is an unbuffered rendezvous — an answer nobody asked for must
not be queued up to release the *next* dialog — so `answer_dialog` refuses
immediately when no dialog is open and otherwise blocks briefly rather than
using `try_send`: a driver that answers the instant it sees the event can
outrun the page's own `recv`, and dropping the answer for being microseconds
early would strand the dialog until its timeout.

**It must not block on `cmd_rx`.** While parked inside `run_dialog` the page is
deep in a JS call and services no ordinary jobs, so an answer arriving as an
ordinary job would sit behind the very block it is meant to release — a
guaranteed deadlock, not a race. The answer therefore comes over its own reply
channel, and the dialog answer is one of D3's three `control` jobs precisely so
that a driver may also cancel out of it.

The page raises a shared "a dialog is open" flag **before** it announces, and
lowers it after the answer is in, so a driver that answers the instant it sees
`DialogOpening` finds the flag already up. The flag belongs to `Page` because
it is a fact about the page, and because only the page can raise it early
enough.

Two exit paths are mandatory, per ADR-0025's recorded obligation:
`recv_timeout(DIALOG_TIMEOUT)` and sender-disconnect (the driver went away, or
the page was closed), both falling back to `DialogResponse::Dismiss` — the same
answer the no-handler default gives. **The `ScriptBudget` cannot rescue this
one**: it is enforced through the engine's interrupt callback, and the block is
in Rust, not in JS. Nothing else is watching.

### D12. `window.open` is a real, minimal `WindowProxy`

`window.open` does not exist today, and `<a target="_blank">` navigates in place
after recording a warning
(`crates/page/tests/navigation.rs:399::target_blank_navigates_in_place_with_a_warning`).

`WindowProxy` becomes a real IDL interface with `closed`, `close()`, `focus()`
and a writable `location` — the surface scripts actually touch on a popup
handle. Only `_blank` and named targets open one: `_self`, `_parent` and `_top`
all name the single browsing context a page has, so they navigate it in place
and `window.open(url, "_self")` returns that same window. The page is created through one new hook, plain data in and plain data
out for the reason `run_dialog` is (it runs with JavaScript on the stack):

```rust
fn open_window(&self, request: OpenWindowRequest) -> Option<OpenedWindow>;

struct OpenWindowRequest { url: Option<String>, target: String,
                           features: String, opener_url: String }
struct OpenedWindow { closed: Arc<AtomicBool>,
                      ops: Arc<dyn Fn(WindowOp) + Send + Sync> }
enum WindowOp { Navigate(String), Close, Focus }
```

Every member is either an atomic read or a fire-and-forget message; **nothing on
a `WindowProxy` blocks on the sibling's thread.** A getter that did would
deadlock the first time two pages opened each other, so `closed` reads a shared
`AtomicBool` (and `close()` sets it locally as well as sending, because a
browser reports `w.closed === true` on the very next line).

Two consequences of the sibling being a *separate* browsing context on another
thread:

- **Reading `w.location` throws `SecurityError`** — exactly what a cross-origin
  `WindowProxy` does in a browser, and exactly what this is: a context this
  realm cannot synchronously introspect. Writing it navigates the sibling,
  resolved against the *opener's* document as HTML says.
- **`focus()` is reported, not obeyed.** There is no window manager here, so
  focusing a browsing context has no intrinsic effect; the sibling's driver gets
  `PageEvent::FocusRequested` and an embedder with tabs of its own can raise the
  right one. Being told beats a silent no-op (P6).

`None` means **the popup was blocked**, and `window.open` returns `null`. That
is not a stub hiding behind P6, it is the answer every real browser gives under
a popup blocker, it is a documented return value in the HTML spec, and every
script that handles popups already tests for it. A bare `Page` and the CLI have
no hook, so they block popups — an honest, feature-detectable policy.

`<a target="_blank">` routes through the same hook, and keeps today's
warn-and-navigate-in-place behavior when there is no hook. One mechanism, two
entry points.

### D13. Web Storage gets a Rust backend, keeping its JS proxy

`localStorage` and `sessionStorage` are a `bootstrap.js` closure over a `Map`
today (`crates/bindings/src/bootstrap.js:973`). That is per-page-per-realm by
construction and cannot be shared between the pages of a context, which is what
`localStorage` *means*.

`Storage` becomes a real IDL interface backed by `HostData::Storage`. The JS
`Proxy` **stays**, and not for compatibility: it *is* the named-property
surface. `s.foo = 1`, `delete s.foo` and `Object.keys(s)` are `Storage`'s
`[LegacyUnenumerableNamedProperties]` behavior, the code generator has no
support for named property getters/setters/deleters, and the proxy already
implements them correctly against a token-guarded target. It simply forwards to
Rust instead of to a `Map`.

- **`BTreeMap`, not `HashMap`.** `Storage.key(i)` must be stable across calls,
  and a `HashMap` iteration order is neither stable nor reproducible.
- **A navigation re-points the existing `Storage` handles** rather than
  installing new ones, so a reference a script captured before the navigation
  (`window.ls = localStorage`) follows the document to its new origin instead of
  writing the previous one's data. The realm outliving a navigation is what
  makes that possible at all, and it is the one place this engine has to be
  explicit about something a browser gets from replacing the global object.
- **`localStorage` is keyed by (context, origin); `sessionStorage` by page.**
  That is the spec's split and the useful one: two pages of a context share a
  login token, and a per-page scratchpad stays per page. A document with an
  **opaque** origin (`about:blank`, `data:`) is keyed by a per-page token
  instead of by its URL, because an opaque origin shares with nobody — keying
  it by URL would hand every blank page of a context one `localStorage`.
- **The quota is counted in UTF-16 code units**, the unit browsers charge, so a
  page storing CJK or emoji gets the 5 MiB it was promised rather than a third
  of it.
- **`HostHooks` grows exactly one method**, handing back the page's storage
  areas — not one per `Storage` operation. `HostHooks`
  (`crates/bindings/src/state.rs:37`) has three implementations —
  `LoopHooks` in `page` plus the harnesses in `crates/bindings/tests/console.rs`
  and `crates/bindings/tests/bindings.rs` — so every method added costs three
  edits, and a six-method family would cost eighteen. The sibling notification
  that a write must produce lives on the `StorageArea` itself, where the
  subscriber list already is.
- **A 5 MiB quota per area**, over-quota writes throwing `QuotaExceededError`.
  Unbounded storage in a process that runs attacker-controlled content is a
  memory-exhaustion primitive, and the quota is the number every browser uses,
  so pages that handle it are already written for it.

## Consequences

**Nothing existing changes behavior, because every new capability is opt-in.**
The CLI and all 27 files in `crates/page/tests/` are unaffected: no event sink
(`None`), no net configuration (`None` — a `Page` still builds its own
`NetService`), and no command port (a `Page` that is never given a receiver
never enters `run_command_loop`). The three `recv_deadline` call sites become
one `wait_for_work` call each; with no command receiver registered, the `Select`
degenerates to the single-channel wait it replaced.

**`engine` is purely additive.** It is a four-line stub today
(`crates/engine/src/lib.rs`), so there is no migration and no deprecation:
embedders who want one page keep constructing a `Page`, and the layering in
CLAUDE.md gains one crate above `page` with no edge reversed.

**ADR-0004's property is preserved literally.** Not "we still avoid busy-waiting
in spirit" — the number of blocking waits per loop iteration is still exactly
one, and the idle case improves from "return to the embedder" to "park
indefinitely at 0% CPU". D4's disconnect trap is the one way to lose it, which
is why it is written down rather than left to be rediscovered as a CPU graph.

**Design §5.12 is corrected.** Its `Browser`-is-unneeded parenthesis and its
"sharing nothing but the process" claim are superseded here; §7 stands as
written and is what stage 5 implements.

**Verification** (roadmap stage 5): `crates/engine/tests/` — two pages in one
context share cookies and two contexts do not; a command answered while a page
is mid-`settle`; a job sent during a document load runs after it, and a `close`
sent at the same moment runs immediately; an idle page with a live command port
consumes no CPU; a dialog answered over the protocol path, and the timeout and
disconnect fallbacks; `localStorage` shared between sibling pages and isolated
between contexts. No regression in the `geometry_rmw` and `reflow` benchmarks.

## Deliberate limits (P6 — absent beats fake)

- **A task source whose producer is another thread must wake the loop, not only
  leave work behind.** An idle page parks indefinitely, so `Page` carries a
  one-slot waker channel registered in the same `Select`. Today only sibling
  `storage` writes use it; anything added later with an off-thread producer
  needs it too, or its work is delivered only when something unrelated happens
  to wake the page.
- **A job sent while the page is navigating or parsing queues until the top of
  the loop.** Only close, stop-loading and a dialog answer run immediately (D3).
  This is not a latency bug to be fixed later — it is the `BorrowMutError`
  boundary, and a real browser's nested event loop defers ordinary tasks in the
  same situations for the same reason.
- **`PageEvent` carries no network events.** The bus covers lifecycle, console,
  errors and dialogs — the four things the page already funnels. Nothing about a
  request is retained today (`dispatch_net_event` consumes each event inline and
  must call `finish` on every terminal one), and stage 6 needs a bounded
  response-body LRU for `Network.getResponseBody` regardless. Retaining request
  metadata is that stage's job, done once, rather than a half-version here that
  stage 6 would have to replace.
- **`window.close()` is ignored, with a warning.** HTML's close steps return
  early unless script opened the browsing context, and this engine tracks no
  opener — so the check can only fail. Reported rather than silent, which is
  what keeps it out of the no-op category P6 rules out; a sibling is closed
  through its `WindowProxy`, which does have the handle. Leaving the member off
  is worse than either: `window.open('', '_self'); window.close();` is a common
  self-close shim, and an absent member turns its second statement into an
  uncaught `TypeError`.
- **The browser-wide HTTP cache bounds entries, not bytes** (4096). Each entry
  retains a whole response body, and the per-page `max_total_bytes` is a
  *transfer* budget that deliberately is not shared, so nothing bounds retained
  memory. A byte budget belongs on `HttpCache` and is not built here.
- **`window.open` blocks its opener for at most five seconds** while the new
  page is built (`OPEN_WINDOW_TIMEOUT`), then returns `null`. That wait is on
  the opener's page thread with JavaScript on the stack, where the
  `ScriptBudget` cannot fire and no control job can land, so it is a
  script-blocking budget and is sized like one — not the driver's much longer
  command timeout.
- **`window.open` is capped per context** (`BrowserOptions::max_pages_per_context`,
  64 by default). Past the cap it returns `null` — the popup-blocker answer D12
  already defines. Nothing else bounds it: each call spawns an OS thread and a
  whole `Page`, and the `ScriptBudget` is per task, so an uncapped
  `for (;;) window.open()` on attacker-controlled content is a host-exhaustion
  primitive rather than a slow page.
- **No named targets, no `window.opener`, no `postMessage`, no `noopener`.**
  `window.open(url, "x")` called twice opens two pages, where a browser would
  reuse the named one. A named-target registry is only meaningful alongside
  `opener` and cross-page messaging, and those are stage 11's problem
  (`postMessage` is listed there and in this stage's non-goals).
- **`ResourcePolicy` is per browser, not per context** (D8), because the SSRF
  connector is baked into the hyper client. Byte and request budgets go the
  other way and stay per page (D7).
- **The shared HTTP cache has one browser-wide entry cap** (4096), so pages
  compete for it and one context's eviction pressure is observable to another as
  a lower hit rate. `CachePartition` stops one context *reading* another's
  entries (D7); it does not make eviction independent. Per-partition budgets are
  the fix and are not built here.
- **The concurrent-connection ceiling is per page.** `MAX_CONCURRENT_FETCHES`
  (16, `crates/net/src/service.rs`) bounds one page's in-flight fetches;
  there is no browser-wide ceiling, so twenty pages can have 320 requests in
  flight. Adding a second semaphore above the first is easy and is deliberately
  not done blind — the right browser-wide number is a measurement, and hyper's
  own `pool_max_idle_per_host` already bounds the interesting case.
- **An unhandled promise rejection is reported late, not at end-of-task.** A
  browser reports immediately and retracts with `rejectionhandled`; the push bus
  has no retraction, so a rejection is held until the page goes idle or for a
  two-second grace, whichever comes first. A handler attached later than that is
  reported as an error it briefly was.
- **A context — and a page — keeps at most 256 origins' storage**, dropping any
  area no live document still holds, *including* areas that hold data. Sparing
  the non-empty ones would have left the bound unenforced for exactly the case
  it exists for. So an origin's data need not survive until a page returns to
  it, and a driver that needs persistence must read it out
  (`BrowserContext::local_storage`) rather than assume the engine keeps it.
- **Web fonts are deliberately not shared between pages** (D9). The expensive
  part — the system-font scan — already is.
- **Not in scope:** `capi`/cbindgen, a windowed embedder, cross-page
  `postMessage`, `SharedWorker`-style shared anything. The roadmap's own
  non-goals for this stage.
- **"Permissions per context" is empty today.** The roadmap lists permissions
  among the per-context state, but there is no Permissions API in the engine —
  no `navigator.permissions`, no Geolocation, no Notification, nothing that
  could be granted or denied. A `PermissionState` map that no code consults
  would be exactly the always-installed no-op P6 forbids, so it is recorded as
  absent rather than built. It becomes real when the first API that consults it
  does, which is not this stage and is not stage 10 either
  (`Emulation.setGeolocationOverride` is rejected there for the same reason).
