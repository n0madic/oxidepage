# Automation roadmap: headless features → Puppeteer → Playwright

How OxidePage gets from "renders a page" to "a Playwright script drives it
unchanged". Stages are ordered by **what the engine needs first**, not by what
the protocol asks for last: stages 1–4 are engine features that make the CLI and
the library better on their own day, stage 5 is the embedding surface, stages
6–8 land CDP up to a working Puppeteer, stages 9–10 close the Playwright-only
gap. Stage 11 is deliberately after the finish line.

This is a planning document, not a decision record. Each stage names the ADR it
must produce; the ADR is what wins when reality disagrees with this file
(`docs/README.md`).

## The scope rule

**Every stage implements the slice that ~90% of real automation runs touch, and
nothing else.** The remainder is listed per stage as an explicit non-goal.

That is not laziness, it is P6 ("absent beats fake", design §2): an unsupported
CDP method returns a real protocol error and an unsupported Web API is not
installed, so `page.setGeolocation()` fails loudly instead of silently doing
nothing. A driver that reports "not supported" is debuggable; one that lies is
not. The deliberate-limits list at the end is part of the deliverable.

Concretely, the 90% run is: launch → `goto` → wait for a selector → click, type,
select → read text/attributes/`evaluate` → intercept or stub a request → assert →
screenshot or PDF → repeat in a fresh context. Everything in this plan exists to
serve that sentence.

## Where we stand today

Ready to be exposed almost as-is:

| Need | Already in the engine |
|---|---|
| screenshot, PDF | `raster-skia`, `export-pdf`, `--full-page`, `--dpr` |
| viewport / DPR emulation | `Page::set_viewport`, `Viewport { dpr }` |
| user agent | `NavigatorProfile` |
| cookies | RFC 6265bis jar, `NetService::cookies()` |
| console, errors | `Page::drain_console` / `drain_errors` (`crates/page/src/lib.rs:2893`) |
| DOM queries, geometry | arena + `querySelector*` + CSSOM-View surface |
| `evaluate` | `Page::eval` (by value only) |
| load lifecycle | `WaitUntil`, `readyState`, `Page::settle` |

Missing outright: navigation from script, every UI event interface, dialogs,
multiple pages, remote object handles, request interception, isolated worlds,
frames, and the whole `cdp` crate (`crates/cdp/src/lib.rs` is a three-line stub;
so is `crates/engine/src/lib.rs`).

## Stage map

| # | Stage | Unblocks | ADR | Est. |
|---|---|---|---|---|
| 1 | Navigation & session history | "click a link and wait" | yes | 3–4 w |
| 2 | Trusted input (mouse/keyboard/focus/typing) | `click`, `type`, `press`, `hover` | yes | 5–7 w |
| 3 | Dialogs & structured page events | real sites stop throwing on `alert` | no | 1–2 w |
| 4 | Transform-aware geometry, capture completeness | correct click points, `page.pdf()` | yes | 3–4 w |
| 5 | `engine`: Browser, contexts, multi-page, async commands | anything protocol-shaped | yes | 4–5 w |
| 6 | CDP transport + Target/Page/Runtime/Network/Log | **Puppeteer basic green** | yes | 5–7 w |
| 7 | `Input` + `DOM` domains | Puppeteer interaction green | no | 2–3 w |
| 8 | `Fetch` interception, file inputs, downloads | Puppeteer feature-complete (90%) | yes | 4–5 w |
| 9 | Isolated worlds | the gate to Playwright | **yes** | 4–6 w |
| 10 | Frame plumbing + Playwright compat surface | **Playwright green** | yes | 5–7 w |
| 11 | Nested browsing contexts (real iframes) | sites that hide content in iframes | yes | 10+ w |

Estimates assume one experienced engineer and are planning aids, in the spirit
of design §10 — not commitments. Milestone "Puppeteer" is end of stage 8;
milestone "Playwright" is end of stage 10.

---

## Stage 1 — Navigation and session history

**Why first.** Automation is "go somewhere, do something, go somewhere else".
Today `window.location` is installed as getters only
(`crates/bindings/src/lib.rs:1204`), `a.click()` fires the event but does not
follow `href`, `form.submit()` is deliberately absent (ADR-0019), and history is
`pushState` only. Every "click through to the next page" flow is dead regardless
of which protocol sits on top.

**Scope.**

- `location`: `href` setter, `assign`, `replace`, `reload`; a `hash`/fragment
  write is a same-document navigation firing `hashchange`.
- Link activation: a trusted or scripted click on `<a href>` / `<area href>`
  navigates. `target` is honored only after stage 5 (before that, `_blank`
  navigates in place and records a warning).
- Form submission: `form.submit()`, `requestSubmit()`, submit-button activation,
  cancelable `submit` event. `GET` query building plus `POST` as
  `application/x-www-form-urlencoded` and `multipart/form-data` — the `FormData`
  body extractor already exists and is reused, not duplicated.
- **Navigation is a task source, not a call.** Reflow must never re-enter JS and
  a navigating script holds `RefCell` borrows on dom/style/layout, so script
  navigation sets `PageState::pending_navigation` and the event loop drains it,
  exactly like `pending_scroll_targets` today
  (`crates/page/src/lib.rs:1605` gets one more drain step). Same-document
  navigations resolve inline; cross-document ones tear the page down through the
  existing navigation path, including `RenderState::reset()`.
- Session history: an entry list on `Page`, `history.back/forward/go/length`,
  `popstate` for same-document entries, a real refetch for cross-document ones.
- A `NavigationEvent` record on the page —
  `{ Started, SameDocument, Committed, DomContentLoaded, Load, NetworkIdle, Failed }`.
  This is precisely the payload `Page.lifecycleEvent` and `Page.frameNavigated`
  will need in stage 6, and it is testable now from the library.

**Non-goals.** `beforeunload` (and therefore `page.close({runBeforeUnload})`),
the destructive `document.open`/`write` path, bfcache, the Navigation API,
`<meta http-equiv=refresh>` chains beyond a single hop.

**Touch points.** `crates/bindings/src/lib.rs` (location), `imp/html_form_element.rs`,
`imp/html_anchor_element.rs`, `imp/interaction.rs`, `crates/page/src/lib.rs`
(loop + history + `navigate`), `crates/bindings/src/state.rs`.

**Verification.** New `crates/page/tests/navigation.rs` against loopback servers;
WPT `html/browsers/history/` and `html/semantics/forms/form-submission-0/`
subsets added to `xtask wpt`.

---

## Stage 2 — Trusted input: UI events, focus, typing

**Why here.** The single biggest engine gap. No `UIEvent`, `MouseEvent`,
`KeyboardEvent`, `PointerEvent`, `WheelEvent`, `FocusEvent` or `InputEvent`
exists in the IDL at all. `Element.click()` runs activation behavior
(`crates/bindings/src/imp/interaction.rs:49`) but there is no path from a
coordinate to a dispatched event. `Input.dispatchMouseEvent` has nothing to call.

**Scope.**

- The interfaces above, with constructors and init dictionaries — test tooling
  (React Testing Library's `fireEvent`) constructs them directly, and status.md
  already flags their absence.
- A synthesis API on `Page`, which the `Input` domain will map onto one-to-one:
  `dispatch_mouse(kind, x, y, button, buttons, modifiers, click_count)`,
  `dispatch_key(kind, key_def, modifiers)`, `insert_text(&str)`,
  `dispatch_wheel(x, y, dx, dy)`.
- Mouse pipeline: hit-test through the existing `element_from_point`, then the
  real sequences — `mousemove` with the `mouseover`/`mouseout`/`mouseenter`/
  `mouseleave` hover chain; `mousedown` → focus transfer to the nearest focusable
  ancestor (blurring the previous) → `mouseup` → `click` → activation behavior,
  reusing `interaction::click` rather than a parallel path; `dblclick` at
  `detail == 2`; `contextmenu` on the right button.
- `:hover` and `:active` element state. **Trap:** a new state bit needs
  `note_subtree_mutation`, not just a restyle hint, and `select.rs` must defer to
  stylo's `state_flag()` — the same shape as the `:checked`/`:disabled` work in
  ADR-0019.
- Keyboard pipeline: `keydown` → default action → `beforeinput` → value mutation
  → `input` → `keyup`. `Enter` submits a form, `Tab` moves sequential focus in
  DOM order honoring `tabindex`, `Escape` blurs. A US key table (key, code,
  keyCode, text) covering printable ASCII plus the ~30 named keys both drivers'
  keyboard layouts use.
- A text-editing model for `<input>` text types and `<textarea>`: caret and
  selection (`selectionStart`/`End`/`Direction`, `setSelectionRange`, `select()`),
  insert/delete at the caret, `maxlength`, correct `input`/`change` timing. No
  caret is painted — headless — but `type()` and `fill()` are unimplementable
  without the model.
- `Element.scrollIntoView(options)`; wheel scrolls the nearest scrollable
  ancestor through the existing clamped offsets.
- `document.hasFocus()` → always true; a headless page is always focused.

**Non-goals.** Touch and gesture events, `Selection`/`Range` over arbitrary DOM
(only the form-control selection model), `contenteditable`, IME/composition,
drag-and-drop, clipboard, pointer coalescing/prediction, `:focus-visible`.

**Touch points.** New `crates/idl/webidl/uievents.webidl`, new `imp/` modules,
`crates/dom/src/event.rs`, `crates/dom/src/select.rs` + stylo state bits,
`crates/layout/src/geometry.rs` (hit testing), `crates/page/src/lib.rs`.

**Verification.** WPT `uievents/` and `html/user-interaction/focus/` subsets;
`crates/page/tests/input.rs`; an Ahem reftest for `:hover` styling.

---

## Stage 3 — Dialogs and structured page events

**Why here.** Cheap, and it stops real sites from dying. `alert`, `confirm` and
`prompt` do not exist, so a page that calls one throws — before any protocol is
involved.

**Scope.**

- `window.alert/confirm/prompt` implemented as **embedder-mediated**, with the
  default policy both drivers use: auto-dismiss (`alert` returns, `confirm` →
  `false`, `prompt` → `null`) while recording a `DialogEvent` the embedder can
  observe and answer. This is an implementation with documented observable
  behavior and a hook, not a silent stub — P6 holds.
- `ConsoleMessage` grows level, argument *values* (not only the rendered string),
  a source location and a timestamp; JS errors grow parsed stack frames. Both are
  the payloads of `Runtime.consoleAPICalled` / `exceptionThrown` and of
  `page.on('console')` / `page.on('pageerror')`, and both are worth having in the
  CLI on their own.

**Non-goals.** `beforeunload` dialogs, `window.print`, HTTP auth dialogs (they
arrive with `Fetch.authRequired` in stage 8).

---

## Stage 4 — Transform-aware geometry and capture completeness

**Why here.** Both drivers compute the click point from element geometry. ADR-0013
applies `transform` at paint time only and geometry ignores it, so a click on any
transformed element lands somewhere else. Fixing it also yields
`DOM.getContentQuads` for free.

**Scope.**

- Propagate the accumulated transform through the box tree into geometry:
  `getBoundingClientRect`, `getClientRects`, `offset*`/`client*` and
  `elementFromPoint` (which inverse-transforms the probe point). Closes a named
  ADR-0013 limit.
- `Page::content_quads(node) -> Vec<[Point; 4]>` and
  `Page::scroll_into_view_if_needed(node, rect)` — the two primitives every
  actionability check is built from.
- Screenshot completeness: `clip` rectangle, JPEG encoding (the `image` dependency
  already carries the `jpeg` feature), quality.
- **PDF pagination.** `export-pdf` writes one page as tall as the document
  (`crates/export-pdf/src/lib.rs:43`). `page.pdf()` defaults to paginated Letter/A4.
  Add paper size, margins, `printBackground`, `scale`, `landscape`, and slice the
  display list into pages at line-box boundaries — the same "never cut a line in
  half" rule the multi-column work already established (ADR-0016).

**Non-goals.** CSS fragmentation properties (`break-*`, `orphans`/`widows`),
header/footer templates, tagged PDF, WebP screenshot encoding.

---

## Stage 5 — `engine`: Browser, contexts, multiple pages, async commands

**Why here.** This is design Phase 8, and it is the hard prerequisite for any
protocol: today a `Page` is a synchronous single-threaded loop that an embedder
drives by calling `settle(budget)`. A protocol server must deliver commands and
answers *while* the page runs, and push events as they happen.

**Scope.**

- `Browser` → `BrowserContext` → `Page` handles in `crates/engine`. Shared across
  the browser: net pool, HTTP cache (correctly keyed), font collection. Per
  context: cookie jar, Web Storage, permissions, UA/viewport defaults. One OS
  thread per page, as design §7 specifies; handles are `Send` façades over
  crossbeam command channels.
- Event-loop change: `run_until_stalled_until` (`crates/page/src/lib.rs:1605`)
  gains a command task source, and `settle`'s single blocking wait
  (`crates/page/src/lib.rs:1667`) becomes a `crossbeam::Select` over `net_rx` and
  `cmd_rx` with the same deadline. ADR-0004's "one blocking wait, no busy-wait"
  property is preserved — that is the acceptance criterion for this change.
- An outbound `PageEvent` bus (lifecycle, console, errors, dialogs, network) to
  replace the pull-based `drain_console`/`drain_errors` for embedders that want
  push. The pull API stays for the CLI.
- Pages can be created **suspended** — nothing loads or runs until resumed. Stage
  10 needs this for `Runtime.runIfWaitingForDebugger`; the CLI benefits from it
  for init scripts.
- `window.open` creates a page in the same context and returns a minimal `Window`
  handle (`closed`, `close()`, `focus()`, `location` write). Scripts that call
  `w.focus()` on the result are common enough to matter.

**Non-goals.** `capi`/cbindgen (independent of automation), a windowed embedder,
cross-page `postMessage`, `SharedWorker`-style shared anything.

**Verification.** `crates/engine/tests/`: two pages in one context share cookies,
two contexts do not; a command answered while a page is mid-`settle`; no
regression in the `geometry_rmw` and `reflow` benchmarks.

---

## Stage 6 — CDP transport and the Puppeteer core loop

**Milestone: `puppeteer.connect()` → `newPage` → `goto` → `evaluate` →
`screenshot` → `pdf` → cookies, green in CI.**

**Scope.**

- `crates/cdp`: `tokio-tungstenite` WebSocket plus a tiny HTTP endpoint
  (`/json/version` with `webSocketDebuggerUrl`, `/json/list`, `/json/new`).
  Sessions are multiplexed by `sessionId` and **flat mode is implemented from day
  one** — Playwright requires `flatten: true` and Puppeteer tolerates it, so there
  is no reason to build the nested variant.
- New workspace dependencies: `serde`, `serde_json`, `tokio-tungstenite`.
  Message types are generated by `cargo xtask cdp-codegen` from a pinned
  `browser_protocol.json` + `js_protocol.json`, **restricted to an explicit
  allow-list of the domains and members below**. Everything outside it answers a
  uniform `MethodNotFound`, which is both P6 and the drift protection the WebIDL
  pipeline already gives `bindings`.
- Domains:
  - `Browser`: `getVersion`, `close`, `setDownloadBehavior` (deny by default).
  - `Target`: `setDiscoverTargets`, `setAutoAttach`, `getTargets`,
    `createTarget`, `closeTarget`, `activateTarget`, `attachToTarget`,
    `createBrowserContext`, `disposeBrowserContext`, `getBrowserContexts`; events
    `targetCreated/Destroyed/InfoChanged`, `attachedToTarget`,
    `detachedFromTarget`.
  - `Page`: `enable`/`disable`, `navigate`, `reload`, `stopLoading`,
    `getFrameTree`, `getNavigationHistory`, `navigateToHistoryEntry`,
    `setLifecycleEventsEnabled`, `captureScreenshot`, `printToPDF`,
    `bringToFront`, `close`, `handleJavaScriptDialog`; events `lifecycleEvent`,
    `frameNavigated`, `frameStartedLoading`/`frameStoppedLoading`,
    `navigatedWithinDocument`, `loadEventFired`, `domContentEventFired`,
    `javascriptDialogOpening`. All of it is a rename of stage 1's
    `NavigationEvent` and stage 3's dialog events.
  - `Runtime`: `enable`, `evaluate`, `callFunctionOn`, `getProperties`,
    `releaseObject`, `releaseObjectGroup`, `awaitPromise`,
    `runIfWaitingForDebugger`, `addBinding`; events
    `executionContextCreated/Destroyed/ClearedAll` (**with
    `auxData { frameId, isDefault }`** — both drivers key off it),
    `consoleAPICalled`, `exceptionThrown`, `bindingCalled`.
  - `Log`: `enable`, `entryAdded`.
  - `Network`: `enable`, `setExtraHTTPHeaders`, `setUserAgentOverride`,
    `setCacheDisabled`, `getResponseBody`, `getAllCookies`/`getCookies`/
    `setCookie`/`setCookies`/`deleteCookies`/`clearBrowserCookies`; events
    `requestWillBeSent`, `responseReceived`, `dataReceived`, `loadingFinished`,
    `loadingFailed`.
  - `Emulation`: `setDeviceMetricsOverride`, `clearDeviceMetricsOverride`,
    `setEmulatedMedia`, `setUserAgentOverride`.
  - `Security`: `setIgnoreCertificateErrors` (maps to `ResourcePolicy`).
- **The remote object model** is the substantial new engine work. Per execution
  context, an `ObjectStore` maps `objectId → Persistent<JsValue>` with object
  groups and explicit release; `JsRealm` grows persistent-handle support
  (`rquickjs::Persistent` is already used in `crates/js/src/quickjs.rs`).
  `RemoteObject` serialization needs type/subtype/className/description and a
  bounded, cycle-safe `returnByValue` encoder. Both drivers do most of their own
  serialization in-page and pass `objectId`s, which keeps this tractable.
- Response bodies retained per request in a bounded LRU so `getResponseBody` can
  answer; the existing per-page byte budget caps it.
- `oxidepage serve --port N [--allow-private]` in the CLI.
- `cargo xtask puppeteer`: a pinned Node harness in `tests/automation/`, fixtures
  served from a loopback static server (CI never touches the internet, §9), and
  an **expectations file with the same two-sided contract as WPT** — fails on
  regression *and* on unexpected pass.

**Non-goals.** `Debugger`, `Profiler`, `CSS` coverage, `Tracing`, `Accessibility`,
`ServiceWorker`, `WebAudio`, screencast, `Target` for workers.

**Security note.** The endpoint is total remote control of the process. Bind to
loopback only, put a random token in the WebSocket path the way Chrome does, and
document that exposing the port is equivalent to handing over the machine's
network position — the SSRF filter protects the *content*, not the *operator*.

---

## Stage 7 — `Input` and `DOM` domains

**Milestone: Puppeteer's `click`, `type`, `hover`, `select`, `$eval`,
`waitForSelector` green.**

- `Input.dispatchMouseEvent` (including `mouseWheel`), `dispatchKeyEvent`,
  `insertText`. Thin mapping onto stage 2.
- `DOM`: `enable`, `getDocument`, `describeNode`, `resolveNode`, `requestNode`,
  `querySelector`/`querySelectorAll`, `getBoxModel`, `getContentQuads`,
  `scrollIntoViewIfNeeded`, `getFrameOwner`.
- `backendNodeId` ↔ `NodeId`: the mapping **must carry the generation**. The
  arena retires and recycles slots, and a stale id that silently addresses an
  unrelated node is exactly the failure the generation checks exist to prevent.
- `Page.addScriptToEvaluateOnNewDocument` in the main world — trivial once the
  stage 1 navigation lifecycle exists, and it unblocks
  `page.evaluateOnNewDocument`.

**Non-goals.** `DOM.setFileInputFiles` (stage 8), `Input.setInterceptDrags`,
`Input.dispatchTouchEvent`, DOM mutation events over the protocol
(`DOM.childNodeInserted` and friends — inspector features, not automation ones).

---

## Stage 8 — Request interception, file inputs, downloads

**Milestone: Puppeteer covers the 90% run end to end.**

- `Fetch`: `enable` with patterns, `requestPaused`, `continueRequest`,
  `fulfillRequest`, `failRequest`, `authRequired`/`continueWithAuth`.
  - Engine work: `NetService::start_resource` (`crates/net/src/service.rs:159`)
    gains a **pause point** before dispatch and a way to satisfy a request from a
    synthesized response. Every consumer routed through `dispatch_net_event`
    (scripts, sheets, images, fonts, script-initiated `fetch`/XHR) must tolerate
    unbounded delay and a fabricated response — including the parser's
    synchronous blocking loads, which is the sharp edge here.
- `Blob`, `File` and a minimal `FileReader`; `input.files`;
  `DOM.setFileInputFiles`; `Page.setInterceptFileChooserDialog` +
  `Page.fileChooserOpened`. `FormData` stops being strings-only, which also fixes
  real form posts.
- Downloads: `Browser.setDownloadBehavior`, `Page.downloadWillBegin`/
  `downloadProgress`; a navigation to an attachment writes to the download
  directory instead of committing a document.
- `Network.emulateNetworkConditions`: implement `offline` and a simple latency
  model. Bandwidth shaping is rejected explicitly rather than approximated.

**Non-goals.** `Fetch.continueResponse` body rewriting on the response side
beyond `fulfillRequest`, HAR recording, WebSocket interception (no `WebSocket` in
the engine), service workers.

---

## Stage 9 — Isolated worlds

**The gate to Playwright.** Playwright always runs its injected script in a
utility world created with `Page.createIsolatedWorld`; `addInitScript` and
`exposeBinding` ride the same mechanism. Nothing about Playwright works without
it, and nothing about it is cheap.

**Scope.**

- `crates/js`: today `new_realm` builds a whole new `Runtime`
  (`crates/js/src/quickjs.rs:79`), so two realms cannot share objects, GC or a job
  queue. Add `new_world(&realm)` producing a second `JSContext` on the *same*
  `Runtime`.
- `crates/bindings`: `PageState` (`crates/bindings/src/state.rs:548`) splits into
  per-page state (dom/style/layout/net/timers) and **per-world** state (wrapper
  cache, prototypes and interface table, `same_object` cache). `node_to_js` takes
  a world. Pin accounting becomes "a node is pinned while *any* world holds a
  wrapper for it", and finalization reports `(world, tag, data)`. Event listeners
  key on target *and* world — Playwright's injected script installs
  `MutationObserver`s and listeners from the utility world, so "main world only"
  is not an option.
- Custom-element definitions and `custom_wrappers` stay main-world; the utility
  world sees upgraded elements but cannot define new ones.
- `Page.createIsolatedWorld` emits `Runtime.executionContextCreated` with
  `auxData { frameId, isDefault: false }`; `Page.addScriptToEvaluateOnNewDocument`
  grows `worldName`; `Runtime.addBinding` grows `executionContextName`.

**This modifies design §5.3 (wrapper cache identity and the pin contract) and
therefore requires an ADR before a line is written.** The failure mode of getting
it wrong is a wrapper from one world leaking into another, which is a security
boundary in real browsers and a correctness boundary here.

**Non-goals.** `grantUniveralAccess` semantics beyond same-origin (there is one
origin per page until stage 11), per-world CSP, per-world prototype poisoning
protection.

---

## Stage 10 — Frame plumbing and the Playwright compatibility surface

**Milestone: `chromium.connectOverCDP()` and a Playwright test file that clicks,
types, waits, routes and screenshots — green in CI.**

**Scope.**

- A `Frame` model with stable `frameId`s. Only the main frame exists until stage
  11, but the plumbing is real: `Page.getFrameTree`, `frameAttached`,
  `frameNavigated`, `frameDetached`, `frameStoppedLoading`,
  `navigatedWithinDocument`, and every execution context tagged with its frame.
- Playwright-specific protocol behaviors, each of which is a hard requirement:
  - `Target.setAutoAttach { waitForDebuggerOnStart: true, flatten: true }` →
    targets are created suspended (stage 5) and resume on
    `Runtime.runIfWaitingForDebugger`.
  - `Emulation.setFocusEmulationEnabled` — trivially true, but must answer.
  - `Page.setInterceptFileChooserDialog` (stage 8), `Page.setBypassCSP` (accept;
    no CSP is enforced), `Emulation.setEmitTouchEventsForMouse` (**reject** — no
    touch events).
  - `Emulation.setLocaleOverride`: implement as `Accept-Language` +
    `navigator.language`/`languages`.
  - `Emulation.setTimezoneOverride`: **reject with an error.** QuickJS-NG has no
    ICU, so there is no `Intl` and `Date` follows the process timezone; a page
    silently formatting dates in the wrong zone is worse than
    `context({ timezoneId })` failing loudly. Same for
    `Emulation.setGeolocationOverride` — there is no Geolocation API to override.
- An audit pass against Playwright's injected script. Already present:
  `elementsFromPoint`, `checkVisibility`, `getComputedStyle`, `MutationObserver`,
  open `ShadowRoot` + `assignedSlot`, `matches`/`closest`, `getClientRects`,
  `requestAnimationFrame`, `structuredClone`. Known absent and deliberately so:
  `Range`/`Selection` over arbitrary DOM (so `fill()` on `contenteditable` fails),
  `DataTransfer` (so drag-and-drop and `setInputFiles` on non-`<input>` targets
  fail). Each failure must surface as a clear error, not a hang.
- `cargo xtask playwright`: a pinned subset of Playwright's own `page/` specs plus
  a homegrown smoke suite, expectations-tracked exactly like WPT.

**Non-goals.** `browserType.launch()` compatibility (the pipe transport,
Chromium's flag surface and the stderr handshake) — `connectOverCDP` is the
supported entry point and the one worth 90% of the value. Also out: video
recording, tracing, `page.accessibility`, coverage APIs, Chromium-only
`CDPSession` escapes beyond the implemented domains.

---

## Stage 11 — Nested browsing contexts (real iframes)

Deliberately **after** the Playwright milestone: it is the single largest item in
this document, and the 90% automation run does not reach into an iframe.

`HTMLIFrameElement` is an empty interface today
(`crates/idl/webidl/html.webidl:174`) and design §12 lists iframes as an explicit
v1 limit. Loading them breaks the invariant CLAUDE.md states plainly — "there are
many documents, but only one *rendered* one". A real iframe needs: N rendered
documents with `IS_CONNECTED` semantics per browsing context, a realm per frame,
box-tree embedding of a child document, hit-testing and event routing across
frame boundaries, `contentWindow`/`contentDocument` with same-origin checks,
`postMessage`, and per-frame navigation history. Its own ADR and its own phase
plan; sketching it further here would be guessing.

---

## Cross-cutting

**Testing policy.** Automation suites get the same two-sided expectation contract
as WPT: CI fails on regression *and* on unexpected pass, so fixing something
forces the expectation edit into the same commit. Fixtures are served from a
loopback server; CI never reaches the internet. A Node toolchain joins the CI
matrix at stage 6.

**P6 accounting.** Every stage ships its non-goals as documented rejections:
protocol methods answer `MethodNotFound` or a specific error, Web APIs are not
installed. The union of the non-goal lists becomes a "deliberate limits" section
in `docs/status.md` when the Puppeteer milestone lands, so users of the endpoint
can read what will fail before they hit it.

**Security.** The CDP endpoint is remote control of a process that executes
attacker-controlled content. Loopback-only bind, random path token, no default
port. The interaction with `ResourcePolicy` needs an explicit decision: a driver
that can `Fetch.fulfillRequest` can already fabricate any response, so
interception must not become a way around the SSRF filter for *outbound*
connections — `continueRequest` re-validates the URL per hop, like redirects do.

**Order dependencies.** 1 → 2 → 4 are independent of 3 and 5 and can interleave.
6 requires 5. 7 requires 2 + 4 + 6. 8 requires 6. 9 requires 6 and blocks 10. 11
requires 9 and 10.
