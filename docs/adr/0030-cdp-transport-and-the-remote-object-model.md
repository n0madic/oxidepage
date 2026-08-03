# ADR-0030: CDP transport and the remote object model

- Status: accepted
- Date: 2026-08-03
- Builds on: ADR-0022 (navigation), ADR-0025 (dialogs and structured page events), ADR-0026 (geometry and capture), ADR-0027 (browser, contexts, commands)
- Constrained by: design §8 (security), §12 (deliberate v1 limits)

## Context

`crates/cdp` was a four-line stub. Stages 1–5 of `docs/automation-roadmap.md`
built everything a protocol needs and stopped one layer short: navigation
milestones with exactly the shape `Page.lifecycleEvent` wants, a screenshot
options struct written against `Page.captureScreenshot`, dialogs announced
before they block, and a `Send + Sync` `PageHandle` over a permanently `!Send`
`Page`.

What was missing was the protocol itself, plus three pieces of engine work no
earlier stage had a reason to build: a table that keeps a JavaScript value alive
between two commands, retention of request metadata and response bodies, and any
way to enumerate the cookie jar.

The milestone this stage targets is a real driver: `puppeteer.connect()` →
`newPage` → `goto` → `evaluate` → `screenshot` → `pdf` → cookies.

## Decision

### D1 — Message types are hand-written, not generated

The roadmap called for `cargo xtask cdp-codegen` over a pinned
`browser_protocol.json` + `js_protocol.json`, restricted to an allow-list. We
hand-write the params and results instead, one module per domain under
`crates/cdp/src/domains/`.

The drift protection codegen gives `crates/bindings` does not transfer. There,
16 000 lines of glue *call* hand-written implementations, so an IDL change is a
compile error in `imp/` — the generator is load-bearing because the output is
too large to write. Here the implemented surface is ~70 commands, the protocol
version is pinned, the subset is hand-picked, and a handler that disagrees with
its params is already a compile error. Codegen would buy a vendored ~2.5 MB of
JSON and a second code generator to maintain.

The allow-list therefore *is* the `match` arm: a method reaches a handler only
because someone wrote one, and everything else falls through to
`MethodNotFound`.

### D2 — The transport hand-rolls HTTP and does not hand-roll WebSocket

`tokio-tungstenite` (no TLS, no `permessage-deflate`) for the socket; the three
`/json/*` endpoints are raw HTTP/1.1 in `crates/cdp/src/http.rs`, on the model
of `xtask/src/testserver.rs`.

Enabling hyper's `server` feature is a `[workspace.dependencies]` change that
pulls server code into *every* binary — including the CLI's render path — to
serve three GETs. RFC 6455 is the other way round: masking, fragmentation,
ping/pong and the close handshake are real work under hostile input, and a
correctness bug there is a remote crash.

The router **reads** the request head and replays it. An earlier version peeked
without consuming, so `accept_async` could take an untouched stream; that is
wrong twice, because `peek` returns whatever the kernel holds *now* — a request
line split across two TCP segments never completes — and nothing in a peek loop
waits for the rest. `http::PrefixedStream` replays the consumed bytes into the
handshake instead.

### D3 — The object store lives in `PageState`, and the DTOs are what cross

An `objectId` names a live JavaScript value. `JsValue::Object` already *is* a
persistent handle, but it is `!Send` and must drop **before the realm** — the
ordering `Page`'s field order encodes (`state, hooks, realm, net`). A store owned
by a protocol session on the driver's thread would outlive the realm it points
into, and QuickJS aborts the process on a non-empty `gc_obj_list` in
`JS_FreeRuntime`.

So `ObjectStore` is a field of `PageState`, next to `custom_wrappers`
(`crates/bindings/src/remote.rs`), ids are monotonic and never recycled, and the
table is capped at `MAX_REMOTE_OBJECTS`. What crosses the thread boundary is
`RemoteObject` — plain `Send` data holding no `JsValue`, the same rule
`ConsoleMessage` follows (ADR-0025).

`crates/page/src/remote.rs` is the embedder-facing half. It knows nothing about
CDP: the shapes match the protocol's vocabulary, as `ScreenshotOptions` already
did, but they are plain Rust and `crates/cdp` does the JSON.

### D4 — A by-value result is serialized by the realm's own `JSON.stringify`

`returnByValue` travels as JSON *text* produced inside the realm, which
`crates/cdp` parses back into structure.

CDP's by-value serialization *is* `JSON.stringify` semantics: `toJSON` is
honored, cycles throw, `undefined` and functions vanish, property order is the
engine's. A Rust re-implementation would be a second answer to a question the
engine already answers, and it would drift.

The primitives JSON cannot spell — `NaN`, `±Infinity`, `-0`, BigInt — travel in
`unserializableValue` with `value` absent, because `null` would be a different
value.

### D5 — Network retention hooks both fetch paths, not just the async one

`crates/net/src/record.rs` holds the request log and a bounded response-body
store; `NetService` gained an observer and calls it from **both**
`spawn_fetch`'s consumers and `fetch_blocking`.

Hooking only the async path was the obvious mistake to make and would have
missed the main document, every ES module and every blocking `@import` — the
synchronous path produces no `NetEvent` at all. The async half is folded in from
`Page::dispatch_net_event`, the one point every asynchronous response passes
through, because the log and observer belong to the page thread while
`spawn_fetch` runs on a tokio worker.

The store is bounded twice — by count and by total bytes, with a per-body cap —
so a page that streams a gigabyte of images cannot turn a driver's convenience
into an out-of-memory kill. Bodies are dropped **before** the next document is
fetched, not in `reset_for_navigation`: that runs after the fetch, so clearing
there discards the body of the very document just loaded.

`Network.requestId` is `{targetId}.{index}v{generation}`. The engine's
`RequestId` counter is per page, so a bare number collides across targets on one
socket.

Every announced request reports **exactly one** terminal event, and both
directions of that rule have already cost a driver something. Too few: a request
aborted before its headers arrive used to vanish, and since navigation aborts
every pending subresource, each `goto` leaked one from the in-flight count a
`networkidle` wait watches. Too many: `xhr.abort()` is reachable *after* the
response has landed, which appended `loadingFailed` to a request that had
reported `loadingFinished`. `NetService` therefore tracks the open set itself and
closes it through one gate, rather than inferring liveness from whether headers
happened to have been seen.

### D6 — Commands run on lane threads, one per session

`PageHandle::with` blocks, and an ordinary job is deferred while the page is
navigating or parsing (ADR-0027 D3), so a command sent during a `goto` can take
as long as the load. Running that on a tokio worker stalls unrelated I/O;
running it on the socket's read loop stops the connection answering
`Browser.close` while a page spins.

Each session therefore gets one OS thread, one command at a time — preserving
the per-session ordering drivers rely on — plus one lane for browser-level
commands. A lane is created only after the session is validated: it outlives the
command, and `sessionId` comes straight off an untrusted frame, so creating one
first would let any client spawn a thread per invented id.

Two commands take a **priority lane** instead: `Page.handleJavaScriptDialog` and
`Browser.close`. A serial lane makes a dialog answer impossible — the page parks
inside `alert()` while `Page.navigate` still holds the lane, so the answer queues
behind the very command it must unblock, the dialog times out, and the answer
arrives to find nothing showing. That is the canonical
`page.on('dialog', d => d.accept())` shape.

The bar for that lane is **not** "important". One lane is shared by every session
on the connection, so a command that can block holds up every *other* target's
urgent command too: it must be meant to interrupt work in flight **and** be
unable to block itself. `Page.stopLoading` reads as a fit and is not one — the
page thread sits inside a blocking document fetch for the whole of a slow load
and services nothing, so it would occupy the shared lane for seconds and strand
an unrelated target's dialog answer. It runs on its own session's lane, where the
only thing it delays is the session that sent it, and it is a *control* call so
it still answers at the first wait point rather than after the whole navigation.
`Browser.close` can block too (it joins every page thread) and stays anyway,
because nothing queued behind it has anywhere to go.

For the same family of reasons, a driver's **event thread must never block**.
`PageHandle::execution_context_id` is a control call: it reads one `Cell`, and
it is read while translating page events, where an ordinary job deferred by a
navigation would stall every event on that connection for every target.

### D7 — Loopback, a `Host` check, an `Origin` refusal, and a path token

The endpoint is unauthenticated total control of a process that runs
attacker-supplied content. Five defences, in descending order of strength:

1. It binds `127.0.0.1`, with no option to say otherwise.
2. Every request must carry a loopback `Host` — the DNS-rebinding defence.
   Parsing stops at the blank line and a duplicate `Host` is refused: a review
   found that reading headers out of the whole buffer let a page put
   `Host: 127.0.0.1` in a CORS-simple **request body** and walk straight
   through, because the last `Host` won.
3. A request carrying an `Origin` is refused outright — upgrades and `/json/*`
   alike. Browsers apply neither CORS nor a cross-origin block to
   `new WebSocket("ws://127.0.0.1:…")`, and such a request has a genuinely
   loopback `Host` — so without this the `Host` check is no barrier at all. A
   driver never sends `Origin`; a page always does. Chrome closes the same
   vector with `--remote-allow-origins`, which defaults to refusing every
   origin.
4. `/json/new` accepts **`PUT` only**, which is a security boundary rather than
   pedantry, and the reason Chrome moved the endpoint there. `GET` and `POST`
   are CORS-*simple*: any page can issue one cross-origin with no preflight and
   no permission, and an `<img src=…/json/new?url=…>` sends no `Origin` for (3)
   to catch — so a page on the open web could open a target and navigate it from
   the operator's network position, in a loop. A `PUT` cannot be sent without a
   preflight this endpoint never answers. The reply being unreadable protects
   the *response*, not the effect.
5. The WebSocket path carries a 128-bit CSPRNG token.

The token is **not** secret from anything that can reach the port and read a
reply — `/json/version` publishes it, as Chrome's does, because that is how
`puppeteer.connect({ browserURL })` finds the socket. It defends against a blind
scan; (2), (3) and (4) are what defend against a web page. Randomness comes from
`rustls`'s ring provider, already linked through `oxidepage-net`, rather than a
`rand` entry for sixteen bytes.

Resource exhaustion is bounded alongside: a request head has a time limit as
well as a byte limit, concurrent connections are capped, sessions per connection
are capped, and a failed `accept()` backs off — without which descriptor
exhaustion spins the accept loop at 100% CPU on a listener that stays readable.

`Target.createBrowserContext` copies the **default context's** options rather
than starting from the stock defaults. Everything an operator configures on
`oxidepage serve` lives on the default context — the viewport, and
`DialogPolicy::Ask`, without which `page.on('dialog', …)` can never fire — and
`browser.createBrowserContext()` asks for an *isolated* context, not a
differently configured browser. `BrowserContext::options()` exists for this.

### D8 — One world, named twice

Both drivers create a *utility world* while setting up every page. Isolated
worlds are a later stage, and refusing was tried: `browser.newPage()` throws and
nothing works at all.

So `Page.createIsolatedWorld`, `Page.addScriptToEvaluateOnNewDocument` and
`Runtime.addBinding` accept a world name and act on the **main** world. The
script genuinely runs, at genuinely the right time; the only property not
delivered is isolation.

Two details are load-bearing. The named world reports a *distinct* context id
(`ISOLATED_WORLD_ID_OFFSET`), because a driver keys its context map by id and a
duplicate makes the second registration overwrite the first — leaving the main
world with no context, so every `consoleAPICalled` and `bindingCalled` is
dropped by the driver. And every world is re-announced after each commit,
because a new document clears the contexts and a driver re-binds its utility
realm by name.

### D9 — An override the engine cannot perform answers an error

`setUserAgentOverride`, `setTimezoneOverride`, `setGeolocationOverride`,
`setEmitTouchEventsForMouse`, `setEmulatedMedia('print')`,
`setScriptExecutionDisabled(true)`, `setIgnoreCertificateErrors(true)`,
`setCacheDisabled(true)`, `setExtraHTTPHeaders` and `Fetch.enable` are refused
with a message naming *why*.

This is P6 at the protocol boundary. A test that sets a timezone and then
asserts on a formatted date must fail at the setter; `{}` would let it compare
against the wrong zone and blame the assertion. The `false`/no-op form of each
is accepted, because asking for the state that already holds is not a lie.

`Fetch.disable` is accepted for the same reason and is the load-bearing case:
Puppeteer sends it while creating every page.

## Consequences

`crates/cdp` is real and `crates/cli` grew `oxidepage serve`. `page` learned
nothing about protocols — a command is still an opaque
`Box<dyn FnOnce(&Page) + Send>` — and the new engine API (`Page::evaluate`,
`reload`, `navigation_history`, `response_body`, `add_init_script`,
`add_binding`) is protocol-neutral and useful to a direct embedder.

Three engine gaps closed on the way: `CookieJar` had no way to enumerate or
remove, `NetService` retained nothing about a request, and `PageRecord`/
`PageEvent` gained `Network` and `Binding` — the network half being the deviation
ADR-0027 recorded and deferred to this stage.

Two hazards were found by the work and are worth naming. The net observer holds
its hooks **weakly**: `net` outlives `realm` in drop order, and the hooks hold
pending timer callbacks, so a strong reference was a `Persistent` outliving its
runtime and a process abort. And push and pull are alternatives, not a pipeline
— the binding drain must not empty the queue when no event sink is installed, or
the pull API silently loses every payload.

Three deviations from the roadmap's stage-6 text: message types are
hand-written (D1), `Emulation.setUserAgentOverride` is refused rather than
implemented (D9), and `Runtime.addBinding` needed the one-world compromise (D8)
that the roadmap had placed in stage 9.

**Verification.** `crates/cdp/tests/{transport,targets,page,runtime,network,
regressions}.rs` drive a real WebSocket over loopback; `crates/page/tests/
remote.rs` and `crates/bindings/src/remote.rs`'s unit tests cover the object
model against a real realm; `crates/net/src/{record,cookies}.rs` cover retention
and enumeration. `cargo xtask puppeteer` runs a pinned `puppeteer-core` against
the endpoint over loopback fixtures, under the same two-sided expectation
contract as WPT — a regression, an unexpected pass and a stale entry all fail.
20 of its 27 checks pass; the seven that do not are named below.

## Deliberate limits (P6 — absent beats fake)

- **No isolated worlds.** One world under many names (D8). A driver's injected
  helpers are visible to page script, and a page that redefines
  `Array.prototype.map` can perturb them. This is the single largest divergence
  in the stage.
- **`waitForDebuggerOnStart` is accepted but does not suspend the page.** A
  suspended page defers every ordinary job until `resume()`, and a driver sends
  its whole session setup *before* `Runtime.runIfWaitingForDebugger` — so
  suspending does not delay the page, it deadlocks the setup. Honouring the flag
  needs a suspension-safe setup path first.
- **No `DOM` domain.** `describeNode`, `getBoxModel`, `resolveNode` and friends
  are the next stage. Five Puppeteer checks fail on it: `page.$`, `page.$$`,
  `page.$eval`, `page.click`, `page.type`, `waitForSelector`.
- **No `Fetch` interception.** `Fetch.enable` is refused; only `Fetch.disable`
  answers.
- **No `Input` domain.** Stage 2 built the synthesis API; the protocol surface
  for it is the next stage.
- **`page.content()` fails** on a missing `XMLSerializer` — an engine gap, not a
  protocol one.
- **No user-agent override.** A page's navigator identity and its `User-Agent`
  header are fixed at construction; half-doing it would have a page claim one
  identity to script and another to the server.
- **No response timing.** The stack measures no DNS/connect/TLS phases, so
  `Network.Response` carries none rather than zeros a profiler would act on.
- **No `Debugger`, `Profiler`, `Tracing`, `Accessibility`, `ServiceWorker`,
  `WebAudio`, screencast, or worker targets.**
- **`Page.stopLoading` cancels only what is queued.** A document fetch already
  in flight is not interruptible: the page thread is inside it and services no
  job of any kind until it returns. This is why the command runs on its own
  session's lane rather than the shared priority one (D6).
- **Nested session mode is refused.** Flat mode only; an explicit
  `flatten: false` is an error rather than silently served as flat.
