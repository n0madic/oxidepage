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
| screenshot, PDF | `raster-skia`, `export-pdf`, `--full-page`, `--dpr`, `--clip`, JPEG, paginated paper (stage 4, ADR-0026) |
| viewport / DPR emulation | `Page::set_viewport`, `Viewport { dpr }` |
| user agent | `NavigatorProfile` |
| cookies | RFC 6265bis jar, `NetService::cookies()` |
| console, errors, dialogs | `Page::drain_console` / `drain_errors` / `drain_dialog_events` (`crates/page/src/lib.rs`) |
| DOM queries, geometry | arena + `querySelector*` + CSSOM-View surface |
| `evaluate` | `Page::eval` (by value only) |
| load lifecycle | `WaitUntil`, `readyState`, `Page::settle` |
| navigation, history, lifecycle events | stage 1, ADR-0022 |
| multiple pages, contexts, async commands, push events | stage 5, ADR-0027 (`crates/engine`) |

Missing outright: remote object handles, request interception, isolated worlds,
frames, and the whole `cdp` crate (`crates/cdp/src/lib.rs` is a three-line
stub).

## Stage map

| # | Stage | Unblocks | ADR | Est. |
|---|---|---|---|---|
| 1 | Navigation & session history — **landed** | "click a link and wait" | ADR-0022 | 3–4 w |
| 2 | Trusted input (mouse/keyboard/focus/typing) — **landed** | `click`, `type`, `press`, `hover` | ADR-0023 | 5–7 w |
| 3 | Dialogs & structured page events — **landed** | real sites stop throwing on `alert` | ADR-0025 | 1–2 w |
| 4 | Transform-aware geometry, capture completeness — **landed** | correct click points, `page.pdf()` | ADR-0026 | 3–4 w |
| 5 | `engine`: Browser, contexts, multi-page, async commands — **landed** | anything protocol-shaped | ADR-0027 | 4–5 w |
| 6 | CDP transport + Target/Page/Runtime/Network/Log — **landed** | Puppeteer basic green | ADR-0030 | 5–7 w |
| 7 | `Input` + `DOM` domains — **landed** | Puppeteer interaction green | ADR-0031 | 2–3 w |
| 8 | `Fetch` interception, file inputs, downloads — **landed** | Puppeteer feature-complete (90%) | ADR-0032 | 4–5 w |
| 9 | Isolated worlds — **landed** | the gate to Playwright | ADR-0033 | 4–6 w |
| 10 | Frame plumbing + Playwright compat surface — **landed** | **Playwright green** | ADR-0034 | 5–7 w |
| 11 | Nested browsing contexts (real iframes) — **landed** | sites that hide content in iframes | ADR-0035 | 10+ w |

Estimates assume one experienced engineer and are planning aids, in the spirit
of design §10 — not commitments. Milestone "Puppeteer" is end of stage 8;
milestone "Playwright" is end of stage 10.

---

## Stage 1 — Navigation and session history — **landed (ADR-0022)**

**Why first.** Automation is "go somewhere, do something, go somewhere else".
Before this stage `window.location` was installed as getters only, `a.click()`
fired the event but did not follow `href`, `form.submit()` was deliberately
absent (ADR-0019), and history was `pushState` only. Every "click through to the
next page" flow was dead regardless of which protocol sits on top.

**Landed.** The scope below shipped as written, with four additions the
implementation forced and one verification gap:

- `Location` and `History` became **real IDL interfaces**, replacing the
  getters-only object *and* the `bootstrap.js` history closure with its
  `__oxide_setDocumentUrl` native hook (both deleted). Cross-origin writes are
  allowed on `Location`; the same-origin check lives on `pushState`.
- `Page::navigate` / `load_html` / `load_document` became **`&self`** — a public
  API signature change, forced by draining the navigation queue from inside
  `run_until_stalled_until(&self)`.
- An **event-handler IDL attribute returning `false` now cancels the event**
  (HTML's event handler processing algorithm step 5). Not a navigation feature —
  a long-standing dispatch bug that was invisible until there was a default
  action to cancel. Without it, WPT's
  `Event-dispatch-single-activation-behavior.html` navigated away from itself
  (`onsubmit="…; return false"`) and reverted to a whole-file HARNESS TIMEOUT.
- **`<label>` activation forwarding**, which the plan listed as out of scope.
  Omitting it *regressed* six passing subtests: a no-op activation behavior is
  not the same as no activation behavior, so without a `<label>` variant a label
  stopped shadowing its ancestors and a click inside one activated the wrong
  element.
- **Verification gap:** WPT's `html/browsers/history/` and
  `html/semantics/forms/` are **not** in the runner's `RUN_DIRS`. Neither
  directory is vendored under `tests/wpt/vendor/`, and fetching them needs
  network, which CI never has (§9). Adding them is a separate, mechanical
  vendoring change; until then the coverage is the Rust integration suites plus
  the activation subtests already vendored under `dom/events`.

The deliberate v1 limits are enumerated in ADR-0022's Consequences; the
non-goals below all held, and `<meta http-equiv=refresh>` was dropped entirely
rather than kept to a single hop. The original scope follows, for the record.

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
subsets added to `xtask wpt`. *(The WPT subsets did not land — see the
verification gap above.)*

---

## Stage 2 — Trusted input: UI events, focus, typing — **landed (ADR-0023)**

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
  ADR-0019. *(The trap was already disarmed: `select.rs` defers generically, so
  no change was needed there. `set_hovered`/`set_active` mirror `set_focused`
  and re-derive whole ancestor chains, because `:hover` matches ancestors.)*
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
  *(Landed as true for the rendered document and false for one with no browsing
  context, which keeps it honest rather than constant.)*

**Non-goals.** Touch and gesture events, `Selection`/`Range` over arbitrary DOM
(only the form-control selection model), `contenteditable`, IME/composition,
drag-and-drop, clipboard, pointer coalescing/prediction, `:focus-visible`.

**Touch points.** New `crates/idl/webidl/uievents.webidl`, new `imp/` modules,
`crates/dom/src/event.rs`, `crates/dom/src/select.rs` + stylo state bits,
`crates/layout/src/geometry.rs` (hit testing), `crates/page/src/lib.rs`.

**What landed beyond the plan.** Three things the work forced rather than chose:

- **Event handler IDL attributes fired only in the target phase**, so `onclick`
  delegation on a container — and `onclick="…"` on any ancestor — never worked.
  A pre-existing bug far larger than this stage, and the single biggest source
  of the +179 subtests.
- **Activation moved into `dispatch_event`** rather than being reused from
  `interaction::click` as the plan said. Triggering on the spec's real condition
  (a `click` carrying a mouse payload) gives one path for `.click()`,
  `dispatchEvent(new MouseEvent(...))` and synthesis, instead of two that agree
  by inspection.
- **`javascript:` URLs**, which ADR-0022 had left warn-and-skip. Once activation
  reached a link that gap became a hang, so it had to close.

**Deviations.** `behavior: "smooth"` is instant (no animation timeline).
`:hover` is verified with `getComputedStyle` after a synthesized move rather
than as an Ahem reftest — the reftest runner has no way to synthesize input, and
extending it for one test was not worth it. Six subtests of
`Event-dispatch-single-activation-behavior.html` are knowingly accepted failures
in a nested-form shape the parser cannot produce; the Chrome behavior they
depend on is recorded in ADR-0023 rather than guessed at.

**Verification.** WPT `uievents/` and `html/user-interaction/focus/` subsets;
`crates/page/tests/input.rs`; an Ahem reftest for `:hover` styling.

---

## Stage 3 — Dialogs and structured page events — **landed (ADR-0025)**

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

**What landed beyond the plan.** Console **format specifiers**
(`%s %d %i %f %o %O %c %%`) and the missing
`console.trace/assert/dir/group/groupCollapsed/groupEnd`, both at the user's
request. Three things the structure then made free: an error carries its `name`
(the old rendering dropped it, so a `TypeError` reached the CLI as a bare
sentence); a script-budget abort names the function that looped instead of
reporting an opaque `InternalError`; and `console.log` of an object shows the
object rather than `[object Object]`. The signature change also forced ~11
open-coded `report_error(error.to_string())` sites in `crates/bindings/src/lib.rs`
through one helper, so they all report `Callback` with real frames.

**Deviations.** Two additions to the plan, both recorded in the ADR: a fourth
engine primitive (`JsScope::symbol_description`, because `ToString` on a symbol
throws and the preview needs the description), and a fifth `ScriptErrorKind`
(`Resource`, because the ~15 event-loop sites that report a stylesheet 404 or an
unresolvable module specifier are none of the four the plan named, and filing
them under `Uncaught` would make `kind` untrustworthy). One accepted spec
divergence: `alert(undefined)` shows `""` where Chrome shows `"undefined"`, the
code generator having no overload support.

**Non-goals.** `beforeunload` dialogs, `window.print`, HTTP auth dialogs (they
arrive with `Fetch.authRequired` in stage 8). Also deferred, and a real P6 hole
recorded as such: `ErrorEvent`/`window.onerror` and
`PromiseRejectionEvent`/`unhandledrejection` *dispatch* — errors reach the
embedder, not page script.

**Verification.** `crates/page/tests/dialogs.rs`,
`crates/bindings/tests/console.rs`, `crates/page/tests/console.rs`,
`crates/js/tests/quickjs.rs`. WPT cannot cover this stage: the relevant
directories are not vendored (ADR-0025, Consequences).

---

## Stage 4 — Transform-aware geometry and capture completeness — **landed (ADR-0026)**

**Why here.** Both drivers compute the click point from element geometry. ADR-0013
applied `transform` at paint time only and geometry ignored it, so a click on any
transformed element landed somewhere else. Fixing it also yielded
`DOM.getContentQuads` for free.

**Landed.** The scope below shipped as written, with the corrections noted under
"Deviations".

- The transform resolver **moved down** from `paint` into
  `crates/layout/src/transform.rs`, so paint, geometry and hit testing share one
  matrix; `layout` caches it per transformed box after rounding, because geometry
  has no access to computed styles. `getBoundingClientRect`, `getClientRects`,
  `Page::layout_rect`, IntersectionObserver and `MouseEvent.offsetX/offsetY` are
  transform-aware, and `elementFromPoint` inverse-transforms the probe point.
  Closes a named ADR-0013 limit.
- `Page::content_quads(node) -> Vec<[Point; 4]>` and
  `Page::scroll_into_view_if_needed(node, rect)`, the latter sharing one
  implementation with `Element.scrollIntoView` (moved to
  `crates/layout/src/scroll_into_view.rs`).
- Screenshot completeness: `ScreenshotOptions` with a document-space `clip`,
  `format` (PNG/JPEG) and `quality`; `Page::screenshot_with`.
- **PDF pagination.** `Page::pdf` paginates onto A4 by default, with paper size,
  margins, `printBackground`, `scale`, `landscape` and fit-to-width. Breaks come
  from `layout::pagination`, over the same class-A break points multi-column uses
  (ADR-0016), and the document's content is emitted once as a form XObject that
  every page invokes.
- CLI: `--clip`, `--quality`, `--paper`, `--margin`, `--scale`, `--landscape`,
  `--single-page`, `--no-fit-to-width`, `--no-print-background`, JPEG output, and
  two silent papercuts fixed (`--dpr` never reached `Viewport.dpr`; it was
  accepted and ignored for non-image output).

**Deviations** (all recorded in ADR-0026). `offset*`/`client*` are **not**
transform-aware, contrary to the bullet above as originally written: CSSOM-View
defines them on the untransformed border/padding box, and
`HTMLImageElement-x-and-y-ignore-transforms.html` passes today *because* we ignore
transforms there. The individual `translate`/`rotate`/`scale` properties were
added (ADR-0013's second named limit) since they share the resolver.
`printBackground` defaults to `true`, unlike Chrome, so `render -o page.pdf` keeps
meaning "the page as it looks". Pagination's fill breaks at the page boundary when
a page holds no break opportunity at all (CSS Fragmentation §3.4's last resort),
where multicol lets a column overflow — without it a `display: flex` body would
print as one page as tall as the document.

**Non-goals.** CSS fragmentation properties (`break-*`, `orphans`/`widows`),
header/footer templates, tagged PDF, WebP screenshot encoding, `@media print`,
relayout at paper width, and a real stacking-context tree for hit-test ordering.

**Verification.** `crates/layout/tests/geometry.rs`, `crates/page/tests/geometry.rs`,
`crates/page/tests/input.rs`, `crates/page/tests/quads.rs`,
`crates/page/tests/screenshot.rs`, `crates/page/tests/pdf.rs`,
`crates/export-pdf/tests/pagination.rs`, a display-list golden
(`tests/goldens/transform.html`), a reftest pair (`tests/reftests/transform-rotate.html`),
and one WPT expectation flipped to PASS
(`css/cssom-view/GetBoundingRect.html :: getBoundingClientRect`).

---

## Stage 5 — `engine`: Browser, contexts, multiple pages, async commands — **landed (ADR-0027)**

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
- Event-loop change: `Page::run_until_stalled_until` (`crates/page/src/lib.rs`)
  gains a command task source, and `Page::settle`'s single blocking wait
  becomes a `crossbeam::Select` over `net_rx` and
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

**What landed beyond the plan.**

- **Commands are boxed closures, not a command enum** —
  `PageJob { control, run: Box<dyn FnOnce(&Page) + Send> }`. `Page::dom()`
  returns a `Ref<'_, DomTree>`, which is neither `Send` nor `'static` and
  physically cannot cross a channel, so an enum protocol would have to
  enumerate every owned projection a caller might ever want, in advance.
  `PageHandle::with(|page| …)` takes the borrow *on the page thread* instead and
  covers the whole tail of the `Page` API, including methods that do not exist
  yet.
- **Web Storage landed in full.** The scope listed it as per-context state
  without saying what that implied; it was an open question, and the answer is
  a real `Storage` IDL interface backed by Rust, `localStorage` per (context,
  origin), `sessionStorage` per page, a real `StorageEvent` delivered to the
  sibling pages of the context, and a 5 MiB per-area quota throwing
  `QuotaExceededError`. The `bootstrap.js` `Proxy` stays, because it *is* the
  named-property surface the code generator cannot express.
- **Dialogs got a dedicated answer channel with two mandatory exits** (timeout
  and sender-disconnect, both falling back to `Dismiss`) — the obligation
  ADR-0025 recorded when it made `run_dialog` synchronous. The answer must not
  ride the command port: a parked page services no ordinary job, so an answer
  queued there would sit behind the very block it is meant to release.
- **Operational surface the driver side needs and the tests use:**
  `Page::request_close`/`suspend`/`resume`/`is_suspended`/`is_idle`, and
  `Page::loop_stats` → `LoopStats`, the counter that *proves* the loop parks
  rather than spins. On the bus, `PageEvent::Crashed` (a page thread panic kills
  the page, not the browser) and `PageEvent::Dropped { count }` — a channel can
  only refuse the newest event where the pull streams drop the oldest, and the
  marker is what keeps that difference from being silent.

**Deviations.**

- **The `PageEvent` bus carries no network events**, contrary to the scope
  bullet. Nothing about a request is retained today, and stage 6 needs a bounded
  response-body LRU for `Network.getResponseBody` regardless — so retaining
  request metadata is that stage's job, done once, rather than a half version
  here that stage 6 would have to replace.
- **No named `window.open` targets.** `window.open(u, "x")` called twice opens
  two pages where a browser reuses the named one. A named-target registry is
  only meaningful alongside `window.opener` and cross-page messaging, and those
  are stage 11's problem — and this stage's non-goals.
- **Reading `w.location` throws `SecurityError`**, exactly as a cross-origin
  `WindowProxy` does — which is what it *is*: a separate browsing context, on
  another thread, that this realm cannot synchronously introspect. A getter that
  blocked on a round trip would deadlock the first time two pages opened each
  other. The write half works, resolved against the opener's document.
- **`w.focus()` is reported, not obeyed.** It reaches the driver as
  `PageEvent::FocusRequested`; focusing a browsing context means something only
  with a window manager, and there is none here. Told rather than silently
  dropped, which is what keeps it inside P6.
- **"Permissions per context" is struck, not deferred.** There is no Permissions
  API in the engine at all — no `navigator.permissions`, no Geolocation, no
  Notification — so nothing could be granted or denied, and a `PermissionState`
  map that no code consults would be exactly the always-installed no-op P6
  forbids. It becomes real when the first API that consults it does, which is
  not stage 10 either.
- **`ResourcePolicy` is per browser, not per context**, because the SSRF
  connector is baked into the shared hyper client and a per-context policy would
  need a client per context — throwing away the connection reuse that motivated
  sharing. Byte and request budgets go the other way and stay per page.
- **The CLI deliberately stays on `Page`.** `crates/cli` does not depend on
  `engine`; it drives one page synchronously through `settle` plus the pull
  streams, exactly as before. `engine` is purely additive and no layering edge
  reversed: a page with no command port, no event sink and no shared net config
  behaves byte-for-byte as it did.

**Non-goals.** `capi`/cbindgen (independent of automation), a windowed embedder,
cross-page `postMessage`, `SharedWorker`-style shared anything.

**Verification.** `crates/page/tests/commands.rs` and `crates/engine/tests/`
(`browser.rs`, `events.rs`, `dialogs.rs`, `window_open.rs`, `storage.rs`, over a
loopback server in `tests/common/mod.rs`): two pages in one context share
cookies and two contexts do not; a command answered while a page is
mid-`settle`; a job sent during a document load runs after it while a close sent
at the same moment runs immediately; an idle page with a live command port
consumes no CPU; a dialog answered over the protocol path, plus the timeout and
disconnect fallbacks; `localStorage` shared between sibling pages and isolated
between contexts. No regression in the `geometry_rmw` and `reflow` benchmarks.

---

## Stage 6 — CDP transport and the Puppeteer core loop — **landed (ADR-0030)**

**Milestone reached.** `puppeteer.connect()` → `newPage` → `goto` → `evaluate` →
`screenshot` → `pdf` → cookies all work against `oxidepage serve`, verified in
CI by `cargo xtask puppeteer` (20 of 27 checks pass; the rest are named below).

**Four deviations from the scope below**, all recorded in ADR-0030:

- **Message types are hand-written, not generated.** `cargo xtask cdp-codegen`
  does not exist. The drift protection codegen gives `bindings` does not
  transfer to ~70 hand-picked commands against a pinned protocol version, and it
  would cost a vendored ~2.5 MB of JSON plus a second generator.
- **`Runtime.addBinding` and `Page.addScriptToEvaluateOnNewDocument` needed the
  one-world compromise this plan had put in stage 9.** Puppeteer creates a
  utility world while setting up *every* page, so refusing a `worldName` makes
  `browser.newPage()` throw. A named world is accepted, reports a distinct
  context id, and acts on the main world.
- **`Emulation.setUserAgentOverride` is refused**, not implemented: a page's
  navigator identity and its `User-Agent` header are both fixed at construction,
  and changing one without the other would have a page claim one identity to
  script and another to the server.
- **`Page.addScriptToEvaluateOnNewDocument`, `Performance`, `IO` and
  `Fetch.disable` arrived early**, because a real driver sends all four while
  creating a page.

The original scope follows, for the record.

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

## Stage 7 — `Input` and `DOM` domains — **landed (ADR-0031)**

**Milestone: Puppeteer's `click`, `type`, `hover`, `select`, `$eval`,
`waitForSelector` green.** Met: `cargo xtask puppeteer` is **33/33** with an
empty expectation file, and six new interaction checks were added to the
harness on the way.

**What landed beyond the plan, and what the plan got wrong.**

- `Page.addScriptToEvaluateOnNewDocument` — the fourth bullet below — was
  already done, early in stage 6. Nothing to do.
- Puppeteer 24 does **not** call `DOM.getContentQuads`, `DOM.getBoxModel` or
  `Page.getLayoutMetrics`: `clickablePoint` is a pure in-page `evaluate` of
  `getClientRects()`. All three ship anyway — this file lists them and
  Playwright (stage 10) uses them — but they are not what unblocked the
  milestone. The load-bearing pair is `describeNode` + `resolveNode`, which
  Puppeteer's `bindIsolatedHandle` decorator round-trips on nearly every
  `ElementHandle` call.
- `DOM.getFrameOwner` is `method_not_found` rather than implemented: there are
  no nested browsing contexts to own a frame, so there is nothing to withhold
  and a driver can feature-detect (ADR-0031 D4).
- `XMLSerializer` was pulled in to close `page.content()` — an engine gap the
  stage-6 ADR named, not a protocol one.
- One engine bug surfaced: `DOMRectList` (and five sibling interfaces with an
  indexed getter) had no `@@iterator`, so `[...el.getClientRects()]` threw and
  `page.click`/`page.type` stayed red after both domains were complete
  (ADR-0031 D6).

The original scope follows, for the record.

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

## Stage 8 — Request interception, file inputs, downloads — **landed (ADR-0032)**

**Milestone: Puppeteer covers the 90% run end to end.** Met: `cargo xtask
puppeteer` is **45/45** with an empty expectation file, and twelve new checks
were added on the way — interception (continue, respond, abort, URL override),
`page.authenticate`, offline emulation, `uploadFile`, `waitForFileChooser`, a
multipart upload asserted on the wire, a download, `Blob`/`FileReader`, and
`response.text()` of the navigation itself.

**What landed beyond the plan, and what the plan got wrong.**

- **`page.goto()` was returning `null`, and the plan only half-diagnosed why.**
  The plan identified `loaderId` (D6a) but placed the mint at the document
  *request*. `Page.lifecycleEvent { name: "init" }` is the **only** event that
  moves Puppeteer's `frame._loaderId`, and `LifecycleWatcher` resolves a
  navigation only once that value has *changed* — so `init` had to carry the new
  loader too, which means minting at `NavigationEventKind::Started`. With the
  request-time mint, a `goto` after any navigation that failed without
  committing hung for the full 30 s, because the committed loader had not moved
  either. Found by the offline check, which is the only one that produces a
  failed navigation followed by a successful one.
- **`Page.fileChooserOpened` must carry a `backendNodeId`.** The ADR first
  recorded it as unread; Puppeteer's `#onFileChooser` calls
  `adoptBackendNode(event.backendNodeId)` immediately and hangs without one.
  That forced the chooser announcement to become a *task source* rather than an
  emit from the click's own stack, because the handle table lives on `Page` and
  the activation runs through the bindings hooks.
- **`Browser.setDownloadBehavior` is context state, not target state.** The plan
  implied per-target application; a driver routinely sends it *before* creating
  a page, and applying it only to the pages that exist made that call a silent
  no-op — the very failure the previous refusal existed to prevent.
- **`<input type=file>` has no default rendering**, so an unstyled one lays out
  0×0 and `page.click` reports "not clickable". Recorded as a limit rather than
  fixed: a picker widget is a layout concern, and inventing a size for a control
  that does not exist is the fake P6 forbids.
- **`xtask`'s test server grew behavioural routes.** It served static files and
  never read a request body, so it could not express an upload target, an
  attachment or a 401 challenge. Three routes under a reserved `/-/` prefix now
  do.
- **The `Content-Disposition` parser is the first in the tree** — the only prior
  occurrence was the multipart *writer*. Path separators are stripped at the
  parse rather than at each call site, and the joined download path is
  re-checked against the directory before a byte is written.

The original scope follows, for the record.

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

## Stage 9 — Isolated worlds — **landed (ADR-0033)**

**Milestone: the gate to Playwright.** Met: `cargo xtask playwright` exists and
is **13/17**, with `chromium.connectOverCDP` → `newPage` → `goto` → `title` →
`evaluate` → `locator().click()` → `addInitScript` → `goBack` → `screenshot` →
`console` all working through a real utility world. `cargo xtask puppeteer` went
from 45/45 to **48/48**, the three new checks being the isolation itself.

**What landed beyond the plan, and what the plan got wrong.**

- **The roadmap's "a second `JSContext` on the same `Runtime`" is not viable, and
  the reason is the opposite of the obvious one.** `Context::with` takes a
  `RefCell::borrow_mut` on the runtime, so entering world B from inside world A
  — which is exactly what synchronous cross-world event delivery *is* — would
  panic; and `Persistent::restore` compares only the runtime pointer, so a
  shared runtime would let a world-A wrapper restore silently into world B, the
  one failure the stage exists to prevent. One runtime per world makes nesting
  legal and turns that leak into a typed error for free (ADR-0033 D1).
- **A latent stack bug had to be fixed first.** rquickjs's `update_stack_top` is
  compiled out without the `parallel` feature, so a realm measured its native
  stack budget from wherever it was *created* — measured at **1.53x** the
  intended depth for a realm created 512 KiB down. Harmless with one realm
  created at startup; with worlds created deep inside jobs and callbacks, it is
  a stack overflow. `QuickJsRealm::anchor_stack` (D2).
- **The drop order is the real hazard, and counting `Rc`s does not work.**
  `Page` deliberately keeps its own `Rc<WorldState>` and the realm holds a third
  as `Rc<dyn Any>`, so dropping one handle frees nothing;
  `WorldState::release_js` empties the containers instead, and
  `WorldTable::teardown` does it for **every** world before freeing **any**
  runtime. Found by `dropping_a_page_with_live_worlds_is_clean`, which caught a
  real `JS_FreeRuntime` abort twice — once from `history.state` still being a
  live `JsValue` on page-level state, once from a remote handle filed in the
  wrong world's store.
- **`objectId`s had to become page-unique.** The plan said `callFunctionOn`
  takes the world from the handle; it also has to take it from
  `executionContextId` when there is no handle, which is what Puppeteer sends.
  Both present and disagreeing is an error.
- **Three things the plan did not list as per-world were.** `navigator.languages`
  / `plugins` / `mimeTypes` cached their wrappers on the *shared* `NavigatorData`
  (a cross-world leak and a teardown hazard); promise settling read the value in
  the main world, so a utility-world `await` reported `undefined`; and the event
  loop pumped only the main world's job queue, so a promise created in a utility
  world never settled at all — which is where a driver's entire injected surface
  lives.
- **`MutationObserver` needed a task source of its own.** A mutation queues the
  compound microtask on the queue of the world that *made* it, so every other
  world's observers were never told. Per-world delivery at the task boundary.
- **Two protocol gaps were in the way of Playwright and are unrelated to worlds:**
  `Target.getTargetInfo` did not exist (Playwright sends it first, so nothing
  worked), and every page target omitted `browserContextId` for the default
  context — Playwright asserts on it. Chrome reports both; hiding the default
  context from `Target.getBrowserContexts` is what actually protects it.
- **`Emulation.setEmulatedMedia` now accepts each feature's default.** Playwright
  sends `prefers-color-scheme`, `prefers-reduced-motion`, `prefers-contrast` and
  `forced-colors` while creating **every** page. Accepting the value that already
  holds is ADR-0030 D9's own rule; any other value is still refused.

**Known flake — diagnosed and fixed in stage 10.** `context.newPage`
occasionally timed out on a loaded machine, and because every other check needs
a page, one timeout reported as sixteen. It was **not** a harness sensitivity:
`Target.attachedToTarget` was emitted by the connection's event thread and
raced the `Target.createTarget` reply, which Chrome guarantees it precedes.
Playwright reads `_crPages.get(targetId)._page` the instant that reply lands,
so losing the race is a `TypeError` on `undefined`. See ADR-0034 D7.

**Still failing at the end of stage 9** (`tests/playwright/expectations.tsv`):
`page.fill`, `page.setContent`, `page.waitForSelector` and
`page.exposeBinding`. All four are fixed in stage 10, and the file is now
empty — but the *explanation* recorded here for them ("injected-script plumbing
that stage 10's frame work brings") was wrong about every one. See stage 10's
"what the plan got wrong".

The original scope follows, for the record.

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

## Stage 10 — Frame plumbing and the Playwright compatibility surface — **landed**

**Milestone: `chromium.connectOverCDP()` and a Playwright test file that clicks,
types, waits, routes and screenshots — green in CI.** Reached:
`cargo xtask playwright` is **17/17 with an empty `expectations.tsv`**, there is
a CI job for it, and `cargo xtask puppeteer` stayed 48/48 throughout. Decisions
and deliberate limits: **ADR-0034**.

### What landed beyond the plan, and what the plan got wrong

The four remaining failures were attributed above to one cause — "Playwright's
injected-script plumbing that stage 10's frame work brings". **That was wrong
about all four**, and reading `playwright-core@1.54.1`'s sources against the
engine gave four independent causes, of which one was architectural:

- **`page.fill`** was not the `Input` domain failing to honour a selection — it
  does honour one, which is why Puppeteer's `page.type` works. Playwright's
  `injectedScript.fill` runs `input.select(); input.focus();`, selection first,
  and focus parked a collapsed caret at the end of the value, erasing it.
- **`page.exposeBinding`** was the one architectural problem, and it had nothing
  to do with injected scripts: `awaitPromise` blocked the session lane, and the
  only command that could resolve the promise was a *later* one on that same
  lane. A deadlock, answered after ten seconds with a pending promise
  serialized as `{}`.
- **`page.setContent`** failed at its first statement: `document.open` and
  `document.close` were not installed, so the whole evaluate threw.
- **`page.waitForSelector`** was a bug in *our own harness* — an empty unstyled
  `<div>` never becomes visible, and `waitForSelector` defaults to
  `state: 'visible'`. It would have failed against real Chrome too.

Three further defects were latent rather than new and surfaced while fixing
those: `callFunctionOn` held its `this` handle across the commit that freed the
handle's runtime (a process **abort**, not a failure); `Target.attachedToTarget`
raced the `Target.createTarget` reply it is supposed to precede — which is the
`context.newPage` flake recorded below, never a harness sensitivity; and a
console message was attributed to the main context whatever world made it, so a
driver silently dropped it.

**`Page.frameAttached`/`frameDetached` are deliberately not implemented**,
against the scope below. Chrome sends neither for the main frame and Playwright
takes the main frame from `Page.getFrameTree`, so with no nested contexts both
would be dead code — the "fake" P6 forbids. They arrive with stage 11.

The original scope follows, for the record.

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

**Landed** (ADR-0035). An `<iframe>` owns a real browsing context: its own
document in one shared arena, its own style and layout engines, its own realm.
`src`/`srcdoc` load, a frame's own subresources are fetched against *its* base
URL and reported as *its* requests, scripts inside run in the frame's realm,
`contentDocument`/`contentWindow` and the window family work, `postMessage`
crosses, the element is a replaced box whose content is spliced into screenshots
and PDFs, input/hit-testing/`:hover`/focus cross into a frame while events do
not, a frame navigates itself, `window.name` and named targets resolve, and the
protocol reports the tree with per-frame loaders and contexts. **Both driver
suites are green** — `cargo xtask puppeteer` 50/50, `cargo xtask playwright`
23/23 — so `page.frames()`, `frame.evaluate()` and `frameLocator()` all work.
Deliberate limits (joint session history, OOPIFs, the rest of `sandbox`, and a
blank frame reporting its embedder's URL) are in ADR-0035.

### What the plan did not predict

Kept because each cost real time, and the next stage's plan will be written by
someone who has not paid it.

- **The seam is insertion, not `src`.** HTML creates a nested browsing context
  when the `<iframe>` is *inserted*, independently of any `src`. Taking that as
  the entry point let the whole model — documents, engines, realms, teardown —
  land and be tested before a single byte was fetched. The plan started from
  `src` and would have brought the network along for the ride.
- **`window.document` was bound to `dom.document()`**, so the first script that
  ran inside a frame silently mutated the *page's* tree. The property is a
  non-configurable data value, which is why a frame navigation now rebuilds the
  frame's realm.
- **`MAIN_WORLD` meant two things.** The constant `WorldId` 0 was read at ~15
  sites as "the default world", but every frame has one and ids must stay
  page-unique. Left alone, it installs `customElements` in exactly one frame.
- **"Which document" is not `node_document`.** A node inside a shadow tree is
  owned by its *shadow root*, so `node_document` can answer with a
  `DocumentFragment`. Routing by it dropped every shadow-scoped `<style>`.
- **`w.location` returning a string** made `w.location.href = url` a silent
  no-op — assignment to a property of a temporary. WPT caught it as two
  timeouts, because a page waiting on the frame's `load` waits forever.
- **`Page.getFrameTree` is not enough for a driver.** Both build their frame set
  **once** and index every later event into it, so a frame appearing after
  attach is invisible without `Page.frameAttached`.
- **A WPT rebaseline can grow and still be an improvement.** Non-PASS lines went
  from 5003 to 6053, and almost all of it is suites whose `<iframe>` fixtures
  never loaded before, now running and reporting pre-existing failures. Nothing
  moved from PASS to a failure. Count what became *visible* separately from what
  broke.
- **Two ADR predictions were wrong, in the cheap direction.**
  `EventTargetKey::Window(FrameId)` was unnecessary (the listener registry is
  per world), and `ResourceTable::merge` needed no id rebasing (the image store
  became page-wide). Both were designed for and then not needed.
- **`frameLocator` was not a frame bug.** It hung while `page.frames()`,
  `frame.evaluate()` and `DOM.describeNode`'s `frameId` all worked, because a
  locator is evaluated in a *utility world of the frame it is in* and
  `addScriptToEvaluateOnNewDocument { worldName }` is a standing order every
  later frame must honour. Reading the wire (`DEBUG=pw:protocol`) found it in
  minutes; two plausible fixes had already been implemented and ruled out by
  guessing. **Capture the traffic before the second guess.**
- **The plan's own site table was the best predictor, and still short.** Every
  entry in it was real; what it missed were the *mirrors* of those entries —
  once "which document" was fixed for style, the same question was still
  answered with the page's for the image-load gate, the base URL of every
  subresource, `fire_element_event`'s world, an `on…` handler's scope chain,
  `getComputedStyle`'s stylist, `@font-face`, and the frame's own navigation
  queue. When you find one "the top document is assumed" site, grep for the
  shape rather than fixing the instance.
- **`page.resolve_url` being dead code was the proof.** The page-wide "resolve
  against *the* document" helper ended with zero callers, and the compiler
  saying so is what confirmed the migration was complete rather than mostly
  complete.

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
