# ADR-0011: AbortController, structuredClone, performance.timing, Intersection/ResizeObserver (v1)

- Status: accepted
- Date: 2026-07-11

## Context

Shadow DOM (ADR-0010) let web-component constructors run, but the target SPA's
Angular app still failed to bootstrap: its runtime feature-detects and calls a
next layer of missing web APIs. Probes on 2026-07-11 confirmed five were
`undefined`: `AbortController`/`AbortSignal` (Angular `HttpClient`),
`IntersectionObserver` (zone.js/polyfills feature-detection),
`ResizeObserver` (Swiper, ×2), `structuredClone`, and `performance.timing`
(OpenReplay). This ADR records their v1 implementations.

## Decisions

**D1 — Abort ordering: state, cancel fetches, then dispatch — no internal
microtask checkpoint.** `signal_abort` sets `aborted`/`reason` (an undefined
reason defaults to a fresh `AbortError` `DOMException`; explicit `null` is
preserved), drains the signal's `pending_fetches` (`hooks.abort(id)` + remove
from `pending_net` + `reject(reason)`, which only queues a promise job), then
dispatches `abort` so `onabort` and listeners run synchronously. The event is
dispatched with `events::dispatch_event` directly, **not** `fire_simple_event`:
the latter runs a microtask checkpoint, which would drain unrelated page
microtasks in the middle of the synchronous `abort()` call (reordering hazard).
The `.catch` reactions instead run at the current task's natural checkpoint
after `abort()` returns. A second `abort()` is a no-op.

**D2 — `fetch` reads `signal`; abort integration is id-based.** `parse_request_init`
brand-checks `init.signal` (non-nullish non-signal → `TypeError`). A pre-aborted
signal rejects the promise synchronously and never starts the request; otherwise
the started request id is pushed to `signal.pending_fetches` and pruned from it
when the fetch settles (the settled `PendingNet::Fetch` carries its signal), so
the list stays bounded across a reused signal's many fetches. `Request.signal`
is a documented follow-up (an omitted `init` on a `Request` input carries no
signal). `AbortSignal.abort()`/`timeout()` are hand-installed statics
(`installLateGlobals`) since the codegen emits no static operations.

**D3 — `structuredClone` uses captured intrinsics; unrecognized prototypes
throw.** A recursive clone with a memo `Map` that breaks cycles and preserves
shared identity (two views over one `ArrayBuffer` stay shared). It handles
primitives, plain/null-proto objects (own enumerable string keys), `Array`,
`Map`/`Set`, `Date`, `RegExp`, `ArrayBuffer`, typed arrays/`DataView` (the
shared buffer clones once through the memo), `Boolean`/`Number`/`String`
wrappers, `Error` subclasses (with `cause`), and `DOMException`. Built-ins are
matched by `instanceof` against captured pristine constructors (not exact
prototype identity), so subclass instances clone via their base class, and array
non-index own enumerable keys are cloned too. Functions, symbol values, and any
object whose prototype is neither `%Object.prototype%`/null nor a recognized
built-in — which catches host/slab objects (DOM nodes) — raise `DataCloneError`. A non-empty `options.transfer` is a `DataCloneError`
rather than a silent no-op (transfer detach is unsupported). All built-ins are
captured at bootstrap time so page script cannot hijack the clone by patching
globals.

**D4 — `performance.timing` records monotonic epoch milestones.** A
`DocumentTiming` struct (epoch ms; `0` = not reached) is filled by the page's
lifecycle via `PageState::mark_timing(TimingMilestone)`, computing timestamps
from `time_origin_epoch_ms + start.elapsed()` (monotonic, never a fresh
`SystemTime` per call). `domInteractive` is stamped when the parser stops,
before deferred scripts run (their runtime counts toward
`domContentLoadedEventStart`, not parsing). v1 collapses
`navigationStart`…`responseStart` into one stamp (synchronous HTML injection has
no distinct network phases; real per-phase URL-navigation timing is a refinement).
The pure-JS user-timing buffers reset per document (keyed on a `navigationStart`
change, since the realm survives navigation), and `measure()` throws
`SyntaxError` for an unknown start/end mark (resolving `PerformanceTiming`
attribute names against `performance.timing`). `unload*`/`redirect*`/
`secureConnectionStart` are hardcoded `0`. `PerformanceTiming` is a real
interface (`[SameObject]` cached wrapper). `mark`/`measure`/`getEntries*`/
`clearMarks`/`clearMeasures` are a pure-JS user-timing layer over
`performance.now()`; navigation/resource/paint entry types read back empty.

**D5 — Observer delivery runs in the page event loop, gated by an O(1)
stamp.** `deliver_observations` is called from `run_until_stalled_until` after
`drain_scroll_events` — **not** in `update_the_rendering` (which only runs with
a pending `requestAnimationFrame`, so observers would starve without animation)
and **not** in `microtask_checkpoint` (which would reflow on every checkpoint).
Returning a "progressed" bool gives initial delivery before `settle()` returns
and lets ResizeObserver-mutation chains converge across loop iterations. The
delivery gate is `ObsGate = (dom.style_version(), dom.structure_version(),
layout.paint_stamp())`: the live DOM versions catch not-yet-reflowed mutations
that `paint_stamp` (which reads the last reflow's versions) misses, and the
paint stamp catches scroll/viewport changes the DOM versions miss. Delivery is
skipped when the gate is unchanged and `observe()` did not set the one-shot
`obs_dirty` flag (cleared after the geometry pass) — using a per-target
`initial_pending` for this would let a permanently boxless target bypass the gate
forever. Three phases keep JS out of the layout borrow: gate → geometry (reflow +
compute plain-Rust entry data in a `flush_layout` closure, first pruning targets
whose node has been freed — a stale `NodeId` would panic the layout query — then
updating each live target's `last`/`initial_pending`) → JS (build wrappers,
invoke callbacks). The gate is re-stamped post-reflow so an unchanged next call
fast-outs.

**D6 — Entry objects are real WebIDL interfaces; sizes are frozen plain
objects.** `ResizeObserverEntry`/`IntersectionObserverEntry` are slab-backed
interfaces with precomputed member wrappers, because polyfills feature-detect
on their prototypes (`'intersectionRatio' in IntersectionObserverEntry.prototype`).
`ResizeObserverSize` is a plain frozen `{inlineSize, blockSize}` object.

**D7 — ResizeObserver box rules.** `contentRect` comes from a new
`LayoutEngine::content_box` returning **element-local** coordinates
(`x = paddingLeft`, `y = paddingTop`, size minus border+padding) as the spec
requires — not viewport-absolute; border box from `border_box`;
`devicePixelContentBoxSize` is the content box × `viewport().dpr`. A target with
a box reports when its chosen box size changes; a target with no box (display:none
or removed) reports `0×0` only if it previously had a non-zero box — an initial
observation on a boxless element waits for a box to appear (matching browsers).

**D8 — IntersectionObserver geometry, shared reflow with RO.** Root rect =
viewport `(0,0,w,h)` (implicit or `Document` root) or the root element's padding
box, expanded by `rootMargin` (% resolved against the root's width for
left/right, height for top/bottom). Target rect = `bounding_client_rect` or a
zero rect when unrendered. A non-Element, non-Document `root` is a `TypeError` (WebIDL
`(Element or Document)?`; `Document` maps to the viewport). Intersection is a
clip (touching edges count as intersecting); ratio is the area fraction (a
zero-area intersecting target → `1.0`). Delivery fires when `(isIntersecting, bucket)` changes or on the initial
observation, where `bucket` = the number of thresholds ≤ the ratio.
`takeRecords()` runs only this observer's geometry phase (updating `last`)
without invoking the callback. `time` is `performance.now()`.

**D9 — Persistent observers, cleared on navigation.** Callbacks/wrappers are
held strongly for the page's lifetime (the accepted `MediaQueryListData`
wrapper-cycle leak class). The `resize_observers`/`intersection_observers`
registries are cleared in `PageState::reset_for_navigation` (along with the
`obs_gate`) so stale `NodeId`s are never delivered against the new document.

## v1 limitations

- `Request.signal` is not plumbed (only `fetch(url, {signal})`).
- `structuredClone` treats any non-plain, non-built-in prototype (including
  ordinary class instances) as uncloneable — deliberately conservative to
  reject host objects; `transfer` is unsupported.
- `performance.timing` has no distinct network phases for injected HTML;
  navigation/resource/paint `PerformanceEntry` types are always empty.
- ResizeObserver has no loop-limit / "undelivered notifications" error; one
  wave is delivered per loop iteration and convergence rides the page loop.
- IntersectionObserver does not clip by intermediate overflow ancestors and has
  no visibility/transform occlusion; `delay`/`trackVisibility` are ignored.

## Consequences

All five APIs are defined and spec-reasonable within the v1 limits. This clears
the next bootstrap blocker layer above Shadow DOM for the target SPA: Angular's
`HttpClient` (AbortController), zone.js/polyfill feature-detection
(IntersectionObserver), Swiper (ResizeObserver), and OpenReplay
(performance.timing) no longer hit `undefined`.
