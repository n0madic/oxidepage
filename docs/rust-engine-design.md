# A Headless Web Engine in Rust — Design & Implementation Plan

Status: architecture baseline. Implementation has moved well past this
document's original "greenfield" state — Phases 0–7 below are done, and
substantial additional work has landed beyond the original phase plan (see
the note before §10). This document stays the architecture rationale (why
the pieces are shaped this way); for what is actually implemented today,
see [`status.md`](status.md), and for the decisions and deviations
recorded along the way, see [`adr/`](adr/) — **ADRs win over this
document where they conflict.**

A small, modular, embeddable web engine for headless automation, offscreen rendering
(screenshots, PDF), and future GUI frontends — built in Rust from production-grade
reusable components rather than as a monolith or as bindings over C libraries.

---

## 1. Purpose & Scope

### 1.1 Goals

- Load real-world web pages: HTML parsing, full DOM, CSS cascade, JavaScript execution,
  subresource loading (scripts, stylesheets, images, fonts) over HTTP(S).
- Produce correct offscreen output: raster screenshots (PNG), vector output (PDF), and a
  backend-neutral display list for debugging and alternative backends.
- Expose browser-grade page APIs to page JavaScript: DOM, events, CSSOM, `getComputedStyle`,
  geometry (`getBoundingClientRect`, `offset*`/`client*`/`scroll*`), timers, fetch/XHR,
  cookies, `MutationObserver`, `requestAnimationFrame`.
- Be embeddable: a clean Rust API, a C ABI for other languages, a CLI, and a Chrome DevTools
  Protocol (CDP) endpoint so Playwright/Puppeteer drive it without client-side changes.
- Be safe by default against hostile content: memory-safe parsing, SSRF-guarded networking,
  sandboxed `file://` access, bounded JS execution.

### 1.2 Non-goals (permanent or long-term)

- Not a general-purpose browser UI; no tabs, chrome, extensions.
- No multi-process site isolation (single-process, multi-threaded).
- No WebGL, WebGPU, WebRTC, media playback.
- No service workers / web workers in v1 (`Worker` is absent, not stubbed).

### 1.3 Primary use cases

1. Headless automation (scraping, testing) driven programmatically or via CDP.
2. HTML-to-image / HTML-to-PDF rendering services.
3. Embedded HTML views in native applications (later, via the GPU backend).

---

## 2. Design Principles

These are the load-bearing decisions. Each addresses a known failure mode of engines
assembled from independent C libraries connected by bridges.

**P1. One DOM, native, single-threaded.**
The DOM is a Rust-owned arena. The HTML parser writes into it, the style system and layout
read from it directly, and JS bindings are thin handles into it. There is no serialization
step between DOM and layout, no second tree, and no JS-side shim state that can go stale.
Engines that store the DOM in a C parsing library and mirror it into JS through a bridge
shim inevitably develop the two-worlds problem: stale wrapper objects, version-check
contracts guarding every native call, and serialize-then-reparse steps between the DOM
and the layout engine. Owning one tree removes the entire problem class.

**P2. Incremental by construction, not cached by exception.**
Every derived structure (computed styles, box tree, fragment tree, display list) is a
retained tree with dirty tracking. A DOM mutation invalidates the smallest affected
subtree. Geometry reads after mutation cost O(dirty subtree), not O(document). Engines
built on non-incremental layout libraries must rebuild the whole styled document on every
mutation and then bolt on geometry caches, rebuild-cost thresholds, and approximate read
modes to survive scripts that read layout in a loop; designing for incrementality removes
that entire heuristic layer.

**P3. Spec-shaped event loop from day one.**
Task queues per task source, microtask checkpoints, and an "update the rendering" step per
the HTML Standard. `async`/`defer` timing, `MutationObserver`, `requestAnimationFrame`,
and (later) `IntersectionObserver`/`ResizeObserver` all attach to this skeleton naturally;
retrofitting it onto a synchronous core is far more expensive.

**P4. One source of truth for observable geometry and style.**
`getComputedStyle`, JS geometry APIs, and paint all read the same computed-style store and
the same fragment tree. No parallel cascade, no "approximately the same" values.

**P5. Backend-neutral display list boundary.**
Layout produces a serializable command list. Raster backends (CPU, GPU, PDF) are dumb
consumers. The list is JSON-dumpable for debugging and golden tests, and it is the seam
that lets a GUI frontend or an alternative rasterizer arrive later as a backend rather
than a rewrite.

**P6. Absent beats fake.**
APIs we don't implement are not installed. Feature detection must work. No always-failing
stubs, no silent no-ops (documented, deliberate exceptions only).

**P7. Conformance is automated.**
Web Platform Tests (WPT) subsets run in CI from the first milestone that can host them.
Correctness is measured against the spec, not against the author's memory of it.

---

## 3. Technology Selection

### 3.1 Language: Rust

- The engine's job is parsing hostile input (HTML, CSS, fonts, images, compressed streams)
  for hours at a time. Memory safety eliminates the dominant browser CVE class at the
  language level rather than by review effort.
- Rust is the only ecosystem with production-grade, *individually reusable* browser
  components: Servo's parser/style crates and Linebender's layout/text/render crates are
  designed to be embedded piecemeal. C++ offers only monoliths (Chromium, WebKit) or
  libraries with hard ceilings (litehtml-class engines: weak cascade, no incremental
  layout). Zig and Go have no comparable ecosystem.
- The Blitz project (html5ever + stylo + taffy + parley + vello, no JS) is an existence
  proof that this exact component assembly works; we follow its integration patterns and
  add the JS/DOM/network layers it lacks.

### 3.2 Component selection

| Concern | Choice | Rationale / notes |
|---|---|---|
| HTML parsing | `html5ever` | Spec-compliant streaming parser (Servo). Passes html5lib-tests. We implement its `TreeSink` over our arena. |
| HTML serialization | `html5ever` serializer | `innerHTML`/`outerHTML` round-tripping. |
| Encoding | `encoding_rs` | Firefox's encoding library; BOM/meta charset sniffing per spec. |
| URL | `url` | WHATWG URL Standard implementation. |
| String interning | `string_cache` / `web_atoms` | Shared with html5ever/stylo (`LocalName`, `Namespace` atoms). |
| CSS cascade | `stylo` (+ `cssparser`, `selectors`) | Firefox/Servo's style engine, published as standalone crates. Real cascade: custom properties, `var()`, media queries, `@supports`, incremental restyle, Shadow DOM support. The single biggest capability differentiator over any lightweight CSS engine. |
| Selector matching | `selectors` | Same crate stylo uses; one `Element` trait impl serves both `querySelector` and the cascade. |
| Block/flex/grid layout | `taffy` | Runs its algorithms over *our* tree via its partial-tree traits; per-node layout caching gives incrementality. Used by Blitz, Bevy. |
| Inline/text layout | `parley` (+ `fontique`, `swash`/`harfrust`) | Line breaking, bidi, shaping, font fallback. Fontique handles system font enumeration and in-memory (`@font-face`) fonts. |
| JS engine | QuickJS-NG via `rquickjs`; optional V8 via `rusty_v8` behind a feature flag | See §3.3. |
| WebIDL parsing (codegen) | `weedle2` | Feeds our own small bindings generator (§5.3). |
| HTTP | `hyper` + `tower` + `rustls` (custom connector) | Custom connector is required for post-DNS SSRF enforcement (§8). HTTP/2, gzip/brotli/zstd via standard tower layers. |
| Cookies | own jar, `psl` for Public Suffix List | Full PSL, not a curated subset. Jar semantics per RFC 6265bis: `SameSite`, prefixes, `HttpOnly`, caps/eviction. |
| HTTP caching | `http-cache-semantics` + own storage | RFC 9111 freshness computation; memory LRU, optional disk store. |
| Raster (CPU, default) | `tiny-skia` | Headless/CI-friendly: no GPU required. Proven quality (resvg's backend). |
| Raster (GPU, optional) | `vello` | For future GUI frontends. Same display list input. |
| PDF | `pdf-writer` | Vector export from the same display list. |
| Raster images | `image` crate | PNG/JPEG/GIF/WebP decode, pure Rust. Decode failures → placeholder, never fatal. |
| SVG | `resvg` | Best-in-class SVG outside browsers; renders to RGBA via tiny-skia. |
| WOFF2/WOFF | `wuff` | Pure-Rust WOFF/WOFF2 → sfnt decoder (nicoburns'); decodes to raw font data for fontique registration. Landed in Phase 7 (ADR-0008); superseded the brotli-repack approach originally sketched here. |
| Async runtime (net side only) | `tokio` | Confined to the network crate; the page event loop is our own scheduler (§5.4). |
| C ABI | `cbindgen` | Stable C header for Go/Python/etc. bindings. |
| CDP transport | `tokio-tungstenite` + tiny HTTP endpoint | WebSocket JSON-RPC per DevTools protocol. |

### 3.3 JavaScript engine decision

Requirements: full modern ECMAScript, embeddable, per-realm memory limits, execution
interruption, small footprint, sane build.

- **QuickJS-NG (default).** ES2023-complete, ~1 MB footprint, native memory caps and
  interrupt handlers (JS budgets come for free), trivial cross-compilation. Slower than a
  JIT engine — acceptable for automation and rendering workloads; page scripts are rarely
  the bottleneck next to layout and network.
- **V8 (optional, feature-gated).** For JS-heavy SPA workloads where throughput matters,
  and it brings WASM. Cost: heavyweight build/binary, more complex embedding.
- SpiderMonkey (`mozjs`) rejected: embedding story is Servo-shaped and the build is the
  worst of the three.

To keep both viable, all bindings target a narrow `JsEngine`/`JsRealm` trait (§5.3). The
WebIDL codegen emits glue against that trait, not against a concrete engine. The trait is
deliberately minimal (host classes, host functions, exceptions, job-queue pumping,
interrupts) — everything else lives on the Rust side.

### 3.4 Alternatives considered and rejected

- **C++20 with existing libraries (lexbor/litehtml/QuickJS/Cairo class)** — no memory
  safety for the hostile-input core; the DOM/JS bridge problem (P1) and non-incremental
  layout (P2) are structural to that assembly; no C++ equivalent of stylo/taffy/parley
  exists as reusable parts.
- **Zig** — no ecosystem; Lightpanda demonstrates the cost (vendors V8 + C libraries).
- **Embedding Servo wholesale** — delivers the functionality but not the control: we would
  inherit Servo's embedding API surface, process model, and release cadence. Rejected as
  the *architecture*, but Servo remains the reference implementation to consult.
- **Extending Blitz directly** — closest call. Rejected for ownership of the DOM crate:
  Blitz's DOM is shaped for its Dioxus use cases; we need bindings-grade DOM internals
  (wrapper identity, observers, event targets) that warrant a from-scratch `dom` crate.
  We deliberately mirror Blitz's stylo/taffy/parley integration patterns.

---

## 4. Architecture Overview

### 4.1 Pipeline

```
                 net worker pool (tokio)
                 fetch · cookies · cache · SSRF guard
                        │  (channel: task source "networking")
                        ▼
   page thread ┌─────────────────────────────────────────────────────┐
   (one per    │  EVENT LOOP  — task queues · microtasks · rAF       │
    Page)      │                                                     │
               │  html5ever ──▶ DOM arena ◀── JS (QuickJS-NG)        │
               │                  │  dirty bits         ▲            │
               │                  ▼                     │ bindings   │
               │            stylo restyle (incremental) │ (WebIDL    │
               │                  ▼                     │  codegen)  │
               │   box tree ─▶ taffy + parley ─▶ fragment tree ──────┼──▶ geometry,
               │                                    │                │    resolved styles
               │                                    ▼                │
               │                              display list ──────────┼──▶ (immutable,
               └─────────────────────────────────────────────────────┘     thread-safe)
                                                    │
                        ┌───────────────────────────┼───────────────────┐
                        ▼                           ▼                   ▼
                   tiny-skia (CPU)             vello (GPU)         pdf-writer
                   PNG screenshots             GUI frontends       PDF export
```

### 4.2 Workspace layout

```
oxidepage/
├── Cargo.toml                  # workspace
├── crates/
│   ├── base/                   # ids, geometry primitives, error types, atoms re-export
│   ├── net/                    # fetch stack: HTTP, cookies, cache, policy, SSRF guard
│   ├── dom/                    # arena DOM, TreeSink, events, MutationObserver, serializer
│   ├── js/                     # JsEngine trait; quickjs backend; v8 backend (feature)
│   ├── idl/                    # WebIDL sources + codegen (build-time via xtask)
│   ├── bindings/               # generated + hand-written glue: DOM/CSSOM/fetch → JS
│   ├── style/                  # stylo trait impls, stylesheet set, computed-style access
│   ├── layout/                 # box tree, taffy driver, parley IFCs, fragment tree
│   ├── paint/                  # fragment tree → display list
│   ├── raster-skia/            # DisplayList → tiny-skia → RGBA/PNG
│   ├── raster-vello/           # DisplayList → vello (feature "gpu")
│   ├── export-pdf/             # DisplayList → pdf-writer
│   ├── page/                   # event loop, Document lifecycle, navigation, Page API
│   ├── engine/                 # public embedding API: Browser, Page, options
│   ├── capi/                   # C ABI (cbindgen)
│   ├── cdp/                    # Chrome DevTools Protocol server
│   └── cli/                    # `oxidepage-cli`: render / dump / eval / serve
├── xtask/                      # codegen driver, WPT runner, reftest runner
└── tests/
    ├── wpt/                    # vendored WPT subsets + expectations
    ├── reftests/               # pixel-compare page pairs
    └── goldens/                # display-list JSON snapshots
```

Dependency direction is strictly downward: `engine → page → {dom, style, layout, paint,
net, bindings} → base`. Raster backends depend only on `paint`'s `DisplayList` types.

> A cargo-feature-gated `rendering`/`gpu`/`cdp`/`pdf` split, for a smaller
> DOM/JS-only automation build, was the original plan here but was abandoned:
> `dom` unconditionally depends on `style` (see ADR-0005, decision 2). The only
> cargo features that exist today are `system_fonts` (`layout`) and `svg`/`webp`
> (`paint`).

---

## 5. Component Design

### 5.1 `base`

- `NodeId`, `StyleSheetId`, `RequestId`: 32-bit index + 32-bit generation (slotmap-style).
- Geometry: `Point`, `Size`, `Rect`, `Transform2D` (euclid or hand-rolled, f32).
- `EngineError` hierarchy; structured, no stringly-typed errors crossing crate boundaries.

### 5.2 `dom` — the arena DOM

The single source of truth for document state.

```rust
pub struct DomTree {
    nodes: Arena<Node>,            // Vec<Slot<Node>> with generation-checked NodeId
    document: NodeId,
    // side tables (sparse; most nodes have none of these):
    listeners: HashMap<NodeId, EventListenerList>,
    observers: MutationObserverRegistry,
    pins: HashMap<NodeId, u32>,    // JS-wrapper pin counts (see §5.3)
}

pub struct Node {
    parent: Option<NodeId>,
    first_child: Option<NodeId>, last_child: Option<NodeId>,
    prev_sibling: Option<NodeId>, next_sibling: Option<NodeId>,
    flags: NodeFlags,              // dirty bits: style, layout, paint; is_connected; …
    data: NodeData,
}

pub enum NodeData {
    Document(DocumentData),
    // Also backs <template> contents and shadow roots (`shadow: Some(mode)`
    // exactly for the latter) — Shadow DOM (ADR-0010) reused this variant
    // rather than adding a new node kind.
    DocumentFragment { host: Option<NodeId>, shadow: Option<ShadowMode> },
    Doctype { name: StrTendril, public_id: StrTendril, system_id: StrTendril },
    Element(Box<ElementData>),
    Text(StrTendril),
    // A Text node for every spec rule (`CDATASection : Text`); kept as its
    // own variant so hierarchy/`:empty`/whitespace checks can't quietly
    // forget it the way matching only `NodeData::Text` would.
    CdataSection(StrTendril),
    Comment(StrTendril),
    ProcessingInstruction { target: StrTendril, data: StrTendril },
}

pub struct ElementData {
    name: QualName,                       // interned (string_cache atoms)
    attrs: Vec<(QualName, StrTendril)>,
    id: Option<Atom>, classes: SmallVec<[Atom; 4]>,   // caches for selector matching
    stylo: StyloElementState,             // computed styles, `style=`, pseudo-class state
    template_contents: Option<NodeId>,    // <template> contents fragment
    shadow_root: Option<NodeId>,          // fragment attached via attachShadow
    // ...plus custom-element lifecycle state, script "already started"/
    // "force async" flags, and other per-feature bookkeeping added since —
    // this is the shape's intent, not a literal current snapshot; see
    // `crates/dom/src/node.rs` for the field-by-field truth.
}
```

Key decisions:

- **Intrusive sibling links, arena storage.** O(1) spec mutation primitives, cache-friendly
  traversal, and `NodeId` is `Copy` — cheap to hand to JS, style, and layout. Generation
  checks make stale ids a clean error, not UB.
- **Mutation API mirrors the spec's algorithms** (`pre_insert`, `remove`, `replace`,
  attribute change), each of which: (a) updates the tree, (b) queues `MutationObserver`
  records, (c) calls the style invalidation hook (stylo restyle hints), (d) sets dirty
  flags up the ancestor chain. There is exactly one code path for every mutation, so
  invalidation can never be forgotten by a caller.
- **html5ever `TreeSink` implemented directly on `DomTree`.** Parsing streams into the
  arena; no intermediate representation. The parser is suspendable: on `</script>` the
  sink yields to the event loop, the script executes (possibly mutating the DOM), parsing
  resumes. A parser-time `document.write` prepends its markup to the still-unconsumed
  input while the tokenizer is suspended at that script (see §12).
- **Events are native.** Capture/target/bubble dispatch, `stopPropagation`,
  `preventDefault`, activation behavior — all in Rust. Listeners hold engine-agnostic JS
  function handles; dispatch calls into the JS realm. Synthetic events from JS
  (`dispatchEvent`) share the same path.
- **Serialization** via html5ever's serializer for `innerHTML`/`outerHTML`; a parse
  fragment entry point implements `innerHTML =`.

### 5.3 `js` + `idl` + `bindings` — JavaScript integration

**Engine abstraction:**

```rust
pub trait JsEngine: 'static {
    type Realm: JsRealm;
    fn new_realm(&self, opts: RealmOptions) -> Result<Self::Realm>;
}

pub trait JsRealm {
    fn eval(&self, source: &str, origin: &str, kind: SourceKind) -> Result<JsValue>;
    fn register_class(&self, def: &HostClassDef) -> Result<()>;   // prototype + methods
    fn wrap_host_object(&self, class: ClassId, data: HostHandle) -> JsValue;
    fn pump_jobs(&self) -> JobsResult;         // promise jobs / microtasks
    fn set_interrupt(&self, cb: Box<dyn FnMut() -> InterruptDecision>);
    fn memory_limit(&self, bytes: usize);
    fn compile_module(&self, ...) -> ...;      // ES modules + import.meta
}
```

`RealmOptions` carries memory caps, stack limits, and time-slice budgets. The QuickJS-NG
backend maps these onto native facilities; the V8 backend uses isolates + interrupts.

**Bindings via WebIDL codegen.** `idl/` holds `.webidl` files copied from the specs for
the supported surface (Node, Element, Document, Event targets, CSSOM, fetch, …). An
`xtask codegen` step parses them with `weedle2` and emits Rust glue: prototype
registration, argument conversion/validation per WebIDL types (including nullability,
overloads we actually use, and dictionary types), attribute getters/setters, and error
mapping to `TypeError`/`DOMException`. Hand-written impls provide the `*_impl` functions
the generated glue calls into.

Why codegen instead of a hand-written shim: hand-transcribed interface shims drift from
the spec independently, one interface at a time. Codegen makes the IDL the checked
contract, catches signature drift at compile time, and gives every interface uniform
argument coercion for free.

**DOM object identity and lifetime.** Each DOM node gets at most one JS wrapper per realm
(wrapper cache: `HashMap<NodeId, WeakJsRef>`), so `===` identity holds. Wrappers hold
`(NodeId, generation)`; every native call validates the generation and throws a clean
`DOMException` on staleness instead of touching freed memory.

Lifetime contract: a live wrapper **pins** its node (pin count in `DomTree`); detaching a
subtree with no pinned nodes frees it immediately; pinned detached nodes (and their
subtrees) survive until their wrappers are GC'd (finalizer decrements the pin). Known
approximation: cross-heap cycles (node → JS listener closure → node) cannot be traced
across the QuickJS/Rust boundary and are reclaimed only at document teardown. This is
documented, bounded (per-navigation), and matches what embedded engines generally do.

**Global scope.** `window` (= global), `document`, constructors, `console`, timers,
`fetch`/`Headers`/`Request`/`Response`, `XMLHttpRequest`, `URL`/`URLSearchParams`,
`crypto.getRandomValues`/`randomUUID` (OS CSPRNG), `performance.now` plus a
`mark`/`measure`/`getEntries*` user-timing layer, `atob`/`btoa`, `TextEncoder`/
`TextDecoder`, `structuredClone` (cycles, typed arrays, `Map`/`Set`/`Date`/`RegExp`,
`DataCloneError` for host objects), `AbortController`/`AbortSignal`,
`localStorage`/`sessionStorage`, the History API (`pushState`/`replaceState`/
`popstate`), `requestIdleCallback`, `customElements`, `ResizeObserver`/
`IntersectionObserver`. Grown well past this v1 sketch — ADR-0011 and ADR-0012 record
what was added and why. No raw host bridges are ever visible to page script — the
generated glue is registered directly on prototypes, so there is nothing to hide
post-install.

### 5.4 `page` — event loop and document lifecycle

Own single-threaded scheduler per the HTML Standard's event loop processing model (no
tokio on the page thread):

- **Task sources** (each a FIFO queue): timers (min-heap by deadline), networking
  (mpsc receiver from `net`), DOM manipulation, script-initiated (e.g. dynamic script
  execution), internal (parser resumption).
- **Task selection**: spec allows source prioritization; v1 uses round-robin with
  timer-deadline respect. Deterministic ordering is a feature for automation — the
  scheduler supports a seeded deterministic mode for reproducible test runs.
- **Microtask checkpoint** after every task and after every callback into JS: drain the
  engine job queue (promises) and our microtask queue (`MutationObserver` delivery,
  `queueMicrotask`).
- **Update the rendering**: when rendering is dirty *and* there is a consumer (pending
  rAF callbacks, an explicit `render()`/`screenshot()` request, or CDP screencast):
  run rAF callbacks → style → layout → paint → publish immutable display list.
- **Lifecycle**: navigation → fetch document → streaming parse (scripts execute at spec
  points; `defer` scripts after parsing; `DOMContentLoaded`; `async` whenever loaded) →
  subresources settle → `load` event.

Embedder-facing drive API (headless determinism). The page thread is fully
synchronous by design (§7) — there is no `async fn` on `Page`; a blocking
`navigate` drives the event loop itself rather than yielding to an executor:

```rust
page.navigate(url, WaitUntil::Load)?;  // WaitUntil: DomContentLoaded | Load
page.run_until_stalled();              // drain tasks that are runnable now
page.settle(Duration::from_secs(2));   // one blocking recv_deadline; see ADR-0004
page.eval_to_string("document.title")?;
```

### 5.5 `net` — fetch stack

Runs on a tokio worker pool; the page thread sees only `RequestHandle`s and completion
tasks on the networking task source.

- **Layered as tower services**: policy check → cache → cookies → redirect handling →
  HTTP client (hyper/rustls). Each layer is independently testable.
- **`ResourcePolicy`** (secure defaults): scheme allowlist pinned across redirects;
  `block_private_hosts` on; `file://` disabled for network-origin documents; optional
  `file_root` sandbox (symlink/`..` escapes rejected; regular files only); per-request and
  per-page byte/count budgets; separate, optional restriction of JS-initiated loads only.
- **SSRF enforcement point** (§8): custom connector resolves DNS itself, filters the
  resolved address set against the policy (loopback, RFC1918, link-local, CGNAT, metadata
  ranges, and numeric-form literals normalized by the `url` crate), and connects only to
  vetted addresses. Redirects re-enter the full pipeline per hop. This closes
  DNS-rebinding and redirect-to-internal by construction.
- **Cookies**: own jar. Full PSL via `psl` crate. RFC 6265bis semantics: `Domain`/`Path`
  matching, `Expires`/`Max-Age` precedence, `Secure`, `HttpOnly` (sent but invisible to
  `document.cookie`), `SameSite` (schemeful, `None` requires `Secure`), `__Host-`/`__Secure-`
  prefixes, control-character rejection, non-secure-overwrite protection, per-domain and
  global caps with oldest-first eviction. The jar is page-scoped and shared by document
  loads, scripts, fetch/XHR, and render-time subresource loads.
- **Cache**: RFC 9111 semantics via `http-cache-semantics`; in-memory LRU keyed by
  (method, URL, Vary), error responses never cached; optional disk layer later. Because
  layout is incremental (P2), the cache is a performance optimization only — correctness
  never depends on it, and no render-time cache-warming choreography is needed.
- **Fetch/XHR semantics**: Request/Response per Fetch Standard subset; referrer computed
  per `strict-origin-when-cross-origin` with sanitization (no userinfo/fragments, HTTP(S)
  only); header validation (CR/LF/NUL rejected); credentials modes control cookie
  send/accept. CORS: enforced for `fetch`/XHR (simple requests + preflight in a later
  phase); tag-initiated loads follow browser no-cors behavior. A policy switch can relax
  CORS for scraping use cases — relaxed is opt-in, spec behavior is the default.
- **Concurrency**: per-host connection cap (6), HTTP/2 multiplexing when offered,
  connection/TLS-session reuse across a page's subresource loads by construction.

### 5.6 `style` — stylo integration

- Implement stylo's `TDocument`/`TNode`/`TElement` traits over the `dom` arena (the same
  `Element` impl serves the `selectors` crate for `querySelector*`). Blitz's
  `blitz-dom` is the reference for trait-impl shape and pitfalls.
- **Stylesheet set**: `<style>`, `<link rel=stylesheet>` (loaded through `net`), `@import`,
  ordered per document position; media query evaluation against viewport/device state;
  stylesheet `disabled` flag participates in the cascade, as CSSOM requires.
- **Restyle**: DOM mutation hooks translate to stylo restyle hints/invalidation sets;
  stylo computes styles incrementally, optionally in parallel via rayon within the style
  pass. Output: `Arc<ComputedValues>` per element, stored in `ElementData.style_data`.
- **`getComputedStyle`**: reads `ComputedValues` and serializes via stylo's own
  value-serialization code — the exact code path Firefox uses. For properties whose
  resolved value requires layout (width/height/inset on laid-out boxes), we return the
  used value from the fragment tree per CSSOM spec, since P4 guarantees it is available
  and current. Custom properties resolve via `var()` natively; `opacity`, percentages,
  and keyword values serialize per spec rather than as authored-value passthroughs.
- **Shadow DOM**: implemented. stylo's native shadow-tree support meant no cascade
  rework was needed: per-root `AuthorStyles` flush to a `CascadeData` side-map read by
  `TShadowRoot::style_data`, so `:host`, `::slotted()`, and `::part()` match through the
  same restyle traversal as document-level styles. Decisions and v1 limits (no event
  retargeting, declarative Shadow DOM, `slotchange`, `:host(...)`, `exportparts`):
  ADR-0010.

### 5.7 `layout` — box tree, taffy, parley

- **Box tree**: built from DOM + computed styles; anonymous box generation
  (block-in-inline splits, table fixups deferred — see §12), `display:none` subtrees
  produce no boxes, `display:contents` handled at box generation.
- **taffy as algorithm library, not as tree owner**: taffy's partial-tree traits run its
  block/flex/grid algorithms directly over our box tree; per-node caches live in our
  nodes. Style inputs are translated `ComputedValues → taffy::Style` once per restyle,
  cached, and invalidated by style dirty bits.
- **Inline formatting contexts**: an IFC box collects inline-level content (text runs
  with per-run computed styles, `inline-block` atomics, images, `br`). parley performs
  bidi, shaping (swash/harfrust), font fallback (fontique), and line breaking;
  the IFC exposes itself to taffy as a measurable leaf. Line boxes and their fragments
  (positioned glyph runs) are retained for paint and for `getClientRects()` — per-line
  fragments, not a single bounding rect.
- **Fragment tree**: the layout result. Border/padding/content box geometry per fragment,
  scrollable overflow (real `scrollWidth`/`scrollHeight`), relative-position offsets,
  absolutely-positioned placement against the correct containing block (including
  percentage widths and `box-sizing` interaction — handled inside the layout engine, not
  by post-hoc repair passes), stacking contexts and paint order, clip/overflow regions.
- **Incrementality**: style-dirty → rebuild affected boxes; layout-dirty → taffy relayout
  with cache reuse from clean subtrees. A geometry read after a mutation relayouts the
  dirty region only. No thresholds, no approximate modes, no last-known-value caches.
- **Floats are laid out** via taffy's `float_layout`, but line boxes do not yet shorten
  around them. See §12 for what exactly is missing.

### 5.8 `paint` — display list

```rust
pub enum DisplayItem {
    Fill        { rect: Rect, radii: BorderRadii, brush: Brush },       // Brush: Solid | LinearGradient | RadialGradient
    Border      { rect: Rect, radii: BorderRadii, edges: [BorderEdge; 4] },
    Image       { dst: Rect, image: ImageId, tile: TileMode, radii: BorderRadii },
    GlyphRun    { font: FontId, size: f32, color: Color, glyphs: Vec<PositionedGlyph>,
                  debug_text: Option<String> },     // shaped glyphs; raster stays dumb
    PushClip    { rect: Rect, radii: BorderRadii },
    PopClip,
    PushLayer   { opacity: f32, transform: Transform2D },
    PopLayer,
}
```

- Built by walking the fragment tree in stacking-context paint order.
- Immutable and `Send` once built; carries an `Arc`'d resource table (decoded images,
  font references) so rasterization can run on any thread.
- `to_json()` for `dump --format display-list` debugging and golden tests (glyph runs include
  `debug_text` so goldens are reviewable; golden comparison can ignore glyph ids where
  fonts differ across platforms).

### 5.9 Raster backends & PDF

- **`raster-skia`** (default): DisplayList → tiny-skia canvas → RGBA buffer → PNG
  (`png` crate). Glyphs rasterized via swash scaler with a per-(font,size) glyph cache.
  Deterministic output per platform given the same font set (reftest requirement).
- **`raster-vello`** (feature `gpu`): same DisplayList → vello scene. Not a v1 milestone;
  the boundary exists so a GUI frontend is a backend, not a rewrite.
- **`export-pdf`**: DisplayList → `pdf-writer`; text as embedded-font glyph runs (subset
  embedding), vector fills/strokes, images embedded at natural resolution.

### 5.10 Images & fonts

- **Images**: decode off-thread in the net/decode pool (`image` for raster formats,
  `resvg` for SVG rasterized at layout-determined size); results are `ImageId`-keyed
  RGBA in the resource table. Failures produce a placeholder command, never abort a render.
  Format features (`webp`, `svg`) are cargo features.
- **Fonts**: fontique collection = system fonts + web fonts. `@font-face` `src:` loading
  through `net` (same policy/cookies), WOFF2/WOFF decode, registration keyed by the
  stylesheet's font-family rules; unicode-range respected by fontique fallback. Font
  data is `Arc`'d into the display-list resource table for backend use.

### 5.11 Geometry & CSSOM read APIs

All JS reads answer from the fragment tree via one internal service:

- `getBoundingClientRect`/`getClientRects` (per-line-box rects), `offsetWidth/Height/Top/Left/Parent`,
  `clientWidth/Height/Top/Left`, real `scrollWidth/scrollHeight` from scrollable overflow.
- `scrollTop`/`scrollLeft`: real scroll offsets stored per scroll container in layout
  state; writes clamp against scrollable overflow and dirty paint (not layout). A small,
  honest scroll model — no smooth scrolling, no scrollbars painted in v1 — but reads and
  writes are real, so scripted scrolling and position probes behave.
- `elementFromPoint` / `elementsFromPoint`: hit-testing walk of the fragment tree in
  paint order (needed by many automation flows).
- Read path: if style/layout dirty bits are set, flush style+layout for the dirty region
  synchronously, then answer. Incrementality (P2) makes this affordable even for scripts
  that interleave geometry reads with DOM writes in a tight loop (the classic
  widget-library initialization pattern that destroys performance on engines with
  whole-document relayout).
- `window.innerWidth/innerHeight/devicePixelRatio`, `screen.*`, and media queries all read
  the same viewport state object, updated by `Page::set_viewport` (which dirties style —
  media queries — and layout).

### 5.12 Embedding API, C ABI, CLI

> **Superseded in part by ADR-0027.** The parenthesis below — that the `Browser`
> indirection "turned out unneeded" and that multiple pages share "nothing but the
> process" — contradicted §7, and §7 is the side that won. It was accurate for the
> scope it was written in (`page` as the whole embedding API: nothing about a page's
> state needs to cross a thread boundary), but the things worth sharing between pages
> are not page state at all — a connection pool, a response cache, a cookie jar, a
> font scan. `crates/engine` now implements §7 as written, and is no longer a stub.
> Constructing a `Page` directly stays fully supported and is what the CLI does; the
> paragraph is kept for the history.

The `page` crate's `Page` is already the embedding API in practice — a page-per-thread
handle constructed directly, not through a separate `Browser` type (that indirection
turned out unneeded: `Page::new` owns its own realm and net access, and multiple pages
are just multiple `Page`s sharing nothing but the process):

```rust
use oxidepage_page::{Page, PageOptions, WaitUntil};
use std::time::Duration;

let mut page = Page::new(PageOptions {
    viewport: Some(Viewport { width: 1280.0, height: 800.0, dpr: 1.0 }),
    policy: Some(ResourcePolicy::default()),   // secure defaults; see §8
    ..PageOptions::default()
})?;

page.navigate("https://example.org", WaitUntil::Load)?;
page.settle(Duration::from_secs(5));
let title = page.eval_to_string("document.title")?;
let png: Vec<u8> = page.screenshot(1.0);       // dpr
let pdf: Vec<u8> = page.print_to_pdf();
```

- `Page` is not `Send`/`Sync` — it owns the JS realm and the arena DOM directly (P1); an
  embedder that wants multiple pages runs one per OS thread, each with its own `Page`.
  This is simpler than the originally sketched command-channel handle, and was possible
  because nothing about the page's state needs to cross a thread boundary once created.
  *(The one-page-per-thread half is permanent — rquickjs is pinned without `parallel`
  and stylo keeps thread-locals. The command-channel handle came back anyway, one level
  up, because a protocol server needs to command a page **while it runs**: ADR-0027 D1.)*
- **`engine`** is the `Send + Sync` façade over those threads — `Browser` →
  `BrowserContext` → `PageHandle`, per §7 and ADR-0027. Landed; no longer a stub.
- **`capi`** and **`cdp`** remain documented stub crates — the rest of Phase 8, and
  Phase 9, in §10.
- **`cli`** (`oxidepage`, `crates/cli`): `oxidepage eval <file|url> [expr]`,
  `dump [--format layout|display-list]`, and `render -o out.{png,pdf,html}` (format inferred
  from the extension, or set with `--format`; folds what this section originally called
  separate `screenshot`/`pdf` subcommands into one, since PNG/PDF/HTML are all "render
  the loaded page"). `dump` is the same fold applied to the debugging output: the
  box tree and the display list are two views of one loaded, settled page, and
  `--format` selects between them (`layout` by default). Common flags: `--viewport`, `--settle-ms`, `--allow-private`,
  `--max-bytes`/`--max-requests`, `--lazy-images`. Doubles as the smoke-test harness; see
  the main [`README.md`](../README.md) for the full reference.

### 5.13 `cdp` — DevTools protocol server

The strategic automation interface: speaking CDP means Puppeteer and Playwright work
without client-side changes.

- Transport: HTTP endpoints (`/json/version`, `/json/list`) + WebSocket per target.
- v1 domain subset, chosen as the minimal set Puppeteer's `connect → newPage → goto →
  evaluate → screenshot → pdf` path exercises:
  `Target`, `Browser`, `Page` (navigate, lifecycle events, captureScreenshot, printToPDF),
  `Runtime` (evaluate, consoleAPICalled, exceptionThrown, execution contexts),
  `Network` (request/response events, response bodies, setExtraHTTPHeaders, cookies),
  `DOM` (getDocument, querySelector, describeNode), `Emulation` (setDeviceMetricsOverride).
- `Input` (mouse/keyboard dispatch into the event system) in a later phase.
- Compatibility is validated in CI by running Puppeteer's and Playwright's own basic
  suites against the endpoint (§9).

---

## 6. Key Data Flows

**Initial document load.**
`navigate(url)` → net fetches document (policy → cache → cookies → HTTP) → bytes stream to
page thread → `encoding_rs` sniff/decode → html5ever streams into DOM arena → parser hits
`<script src>`: speculative prefetch continues scanning ahead and queues fetches for
subresources it can see, while the parser blocks per spec → script arrives → executes
(bindings mutate DOM directly, dirty bits accumulate) → parser resumes → …
→ end of parsing: `defer` scripts in order → `DOMContentLoaded` → pending subresources
settle → `load`. Stylesheets load in parallel and apply via restyle when ready;
script execution blocks on earlier pending stylesheets per spec.

**Mutation → next paint.** JS mutates DOM → mutation path sets style/layout dirty bits +
queues MutationObserver microtask → task ends → microtask checkpoint delivers observers →
"update the rendering" (if a consumer exists): rAF callbacks → stylo incremental restyle
of dirty subtrees → taffy/parley relayout of dirty boxes → repaint dirty stacking
contexts → new immutable DisplayList published.

**Synchronous geometry read.** `el.getBoundingClientRect()` → binding validates
`(NodeId, gen)` → geometry service sees layout-dirty → flush style+layout incrementally →
read fragment → return rect. Cost proportional to the dirty region.

**Screenshot.** `page.screenshot()` → enqueue render request → event loop reaches "update
the rendering" → DisplayList published → rasterized (optionally off-thread) → PNG bytes.

---

## 7. Threading Model

- **Page thread** (one per `Page`): DOM, JS, style (rayon-parallel internally, but
  synchronous from the loop's perspective), layout, paint. The web's single-threaded
  invariant is embraced, not fought.
- **Net/decode pool** (shared, tokio): HTTP, image decode, font decode. Communicates with
  page threads only through task-source channels.
- **Raster**: DisplayList is immutable + `Send`; rasterization runs wherever the embedder
  wants (page thread for simplicity, worker for throughput).
- **Multiple pages**: independent page threads sharing the net pool, HTTP cache
  (keyed correctly), and font collection. No shared mutable DOM/style/layout state.
- `Browser`/`Page` public handles are thread-safe façades over command channels.

---

## 8. Security Model

Threat model: the engine loads and executes fully attacker-controlled content, and is
commonly deployed server-side (SSRF is the marquee risk).

1. **Memory safety**: Rust throughout; `unsafe` confined to vetted dependency internals
   and audited FFI (QuickJS boundary). Fuzzing on all parser surfaces (§9).
2. **SSRF**: single enforcement point in the connector — DNS resolved in-house, resolved
   addresses filtered (loopback, RFC1918, link-local, CGNAT, cloud metadata,
   IPv6-mapped/scoped forms), connections made only to vetted addresses; numeric-literal
   forms normalized by the URL parser before checks. Redirects re-validated per hop.
   Connection reuse is safe by construction: a pooled connection was validated when opened.
3. **Local files**: `file://` requires explicit opt-in; network-origin documents can never
   read local files; `file_root` jail with symlink/`..` rejection, regular files only.
4. **JS containment**: per-realm memory limits, interrupt-based time slicing, no host
   bridges reachable from page script, `crypto` backed by OS CSPRNG. QuickJS-NG (no JIT)
   keeps the attack surface small by default; the V8 feature flag is documented as a
   larger-surface tradeoff.
5. **Header/cookie hygiene**: control-character rejection on outgoing headers and cookie
   names/values; referrer sanitization (strip userinfo/fragment, origin-only cross-origin,
   HTTP(S) only); full PSL for cookie domain and schemeful same-site decisions.
6. **Budgets**: per-page request count, byte totals, per-request size caps, decode size
   caps (image-bomb defense), redirect chain caps.

---

## 9. Testing Strategy

- **WPT** (vendored subsets, `xtask wpt`): html5lib-tests (via html5ever upstream +
  our TreeSink), `dom/`, `html/webappapis/` (event loop, timers), `cssom/`,
  `css/css-flexbox/` + `css/css-grid/` (layout, via reftests), `fetch/`, `cookies/`,
  `url/`, `encoding/`. testharness.js runs once Phase 2 lands (it needs DOM + JS only).
  Expectations files track known-fail; CI fails on regression *and* on unexpected pass
  (forces expectation updates).
- **Reftests**: render page A and reference page B, compare pixels with per-channel fuzz.
  Platform-pinned font set (bundle test fonts) for determinism.
- **Display-list goldens**: JSON snapshots for paint-order/command regressions; cheaper
  and more reviewable than pixels for structural changes.
- **Unit/property tests**: cookie jar (RFC 6265bis vectors), cache semantics, URL/referrer
  computation, arena invariants (proptest: random mutation sequences keep tree links
  consistent), WebIDL conversions.
- **Fuzzing** (`cargo-fuzz`, CI cron): TreeSink mutations from parser output, CSS parsing
  entry, cookie parsing, WOFF2 decode, display-list JSON round-trip. **Not yet built** —
  no `fuzz/` crate exists today; proptest (arena mutation invariants, `crates/dom/tests/
  proptest_mutations.rs`) is the only property-based coverage so far. Attacker-controlled
  parser surfaces (HTML, CSS, WOFF2/WOFF/TTF/OTF, cookies) remain the natural targets
  whenever this is picked up.
- **Integration**: CLI smoke tests; CDP conformance = Puppeteer + Playwright basic suites
  against our endpoint in CI.
- **Benchmarks** (criterion + scenario harness): 10k-node page full pipeline; style
  invalidation storm; read-modify-write geometry loop (the widget-library-init
  pathology); cold vs warm subresource loads. Tracked over time; regressions block merge.

---

## 10. Implementation Plan

Estimates assume 1–2 experienced systems engineers, familiar with Rust but not
necessarily with browser internals; they are planning aids, not commitments.
Phases are sequenced so that every phase ends with something demonstrable and tested.

**Phases 0–7 are done.** They landed close to this original plan — exit criteria below
are what actually shipped, not just what was proposed. Once real pages were being
rendered, though, the work that mattered next wasn't Phase 8/9 — it was closing gaps
real sites hit (custom elements, Shadow DOM, transforms, SPA bootstrap APIs, …). That
work happened outside this phase numbering; see "Landed beyond the original plan"
below. [`status.md`](status.md) is the up-to-date, phase-by-phase account —
treat the entries below as the historical plan, not a live status feed.

**Phase 0 — Skeleton.** Landed.
Workspace, CI (clippy -D warnings, fmt, test matrix), xtask scaffolding, `base` crate,
ADR (architecture decision record) process. *Exit: green CI on empty crates.*

**Phase 1 — DOM core.** Landed.
Arena + Node/ElementData, spec mutation algorithms, html5ever TreeSink + suspendable
parsing, serializer, `encoding_rs` integration, event dispatch skeleton (no JS yet),
MutationObserver record queuing. *Exit: html5lib-tests tree-construction suite passing;
proptest mutation invariants green.*

**Phase 2 — JS + bindings foundation.** Landed (ADR-0003).
`JsEngine` trait + QuickJS-NG backend (rquickjs), WebIDL codegen pipeline (weedle2 →
glue), wrapper cache + pin lifetime contract, core interfaces (EventTarget, Node, Element,
Document, CharacterData, DOMTokenList, basic HTMLElement), `querySelector*` via
`selectors` with our Element impl, event loop v1 (tasks, microtasks, timers), console.
*Exit: WPT `dom/nodes` + `dom/events` subsets running under testharness.js with tracked
expectations; `oxidepage eval` works on local HTML.*

**Phase 3 — Network.** Landed (ADR-0004).
tower-layered fetch stack, SSRF connector, cookie jar + PSL, HTTP cache, redirect/referrer
logic, document loading over HTTP(S), classic scripts (parser-blocking, `async`/`defer`
with real task timing), ES modules (static imports, `import.meta.url`), `fetch`/XHR
bindings, load lifecycle events. *Exit: WPT `fetch/`, `cookies/`, `url/` subsets green
per expectations; SSRF test battery (rebinding, redirect-to-internal, numeric literals)
green; real pages load and run their scripts headlessly.*

**Phase 4 — Style.** Landed (ADR-0005).
stylo trait impls, stylesheet set + loading integration, media queries, restyle hooks
from mutation path, `getComputedStyle` (computed values), CSSOM (`el.style`,
`CSSStyleSheet`, rule mutation, `document.styleSheets`, `disabled`). *Exit: WPT `cssom/`
subset per expectations; computed-style parity checks against Firefox on a corpus of
pages (spot-audited).*

**Phase 5 — Layout & geometry.** *(most technically risky phase)* Landed (ADR-0006).
Box tree generation, taffy partial-tree integration (block/flex/grid), parley IFCs, fragment
tree with stacking contexts + scrollable overflow, geometry service + all JS geometry
APIs, resolved values in `getComputedStyle`, hit testing (`elementFromPoint`), scroll
offsets. *Exit: WPT flexbox/grid reftest subsets at a declared pass rate; geometry
read-modify-write benchmark within budget; `getClientRects` per-line correctness tests.*

**Phase 6 — Paint & raster & PDF.** Landed (ADR-0007).
Fragment-tree → DisplayList (paint order, clips, radii, gradients), tiny-skia backend,
glyph rasterization + cache, `image`/`resvg` decode integration, backgrounds
(size/position/repeat), borders, PNG screenshots, `pdf-writer` export, `dump --format display-list`.
*Exit: reftest suite green on pinned fonts; golden display lists stable; CLI screenshot/PDF
on a real-page corpus reviewed* — the CLI folded `screenshot`/`pdf` into one `render`
subcommand later; see §5.12.

**Phase 7 — Web fonts.** Landed (ADR-0008).
fontique + system fonts, `@font-face` loading through net, WOFF2 decode, unicode-range
fallback, font subsetting in PDF export. *Exit: web-font reftests; PDF text extractable.*

**Landed beyond the original plan.** Found by pointing the engine at real sites (Angular/
Lit/Stencil SPAs, vuejs.org, WordPress-class pages) rather than by following this
document — each has its own ADR with the full decisions and v1 limits;
[`status.md`](status.md) has the prose version of each:

- **Custom elements** (autonomous only) — ADR-0009.
- **Shadow DOM v1** (`attachShadow`, slots, `:host`/`::slotted()`/`::part()`,
  `adoptedStyleSheets`) — ADR-0010.
- **Abort/clone/timing/observers v1** (`AbortController`, `structuredClone`,
  `performance` user timing, `ResizeObserver`, `IntersectionObserver` — pulled forward
  from Phase 10 below once it became clear script needed them immediately, not at
  hardening time) — ADR-0011.
- **Angular/SPA runtime APIs v1** (`DOMParser`, real inert `Document`s, `crypto`,
  `TextEncoder`/`Decoder`, Web Storage, History API, `Response.body` as a
  `ReadableStream`) — ADR-0012.
- **Lazy image loading** (`PageOptions.lazy_images`) — ADR-0014.
- **CSS transforms, containing blocks, inline SVG v1** — ADR-0013.
- **Whole-document visibility & no silent failures v1** (unhandled-rejection reporting,
  `PageOptions.whole_document_visible`) — ADR-0015.
- **CSS multi-column** — ADR-0016.
- **List markers** (`display: list-item`, counter styles) — no dedicated ADR; see
  `docs/status.md`.
- **Real inert documents** (`new Document()`, `DOMParser`, `CDATASection` as its own
  `NodeData` variant) — ADR-0017.
- **Connected wrapper retention** (GC fix for expando properties on connected nodes) —
  ADR-0018.

**Phase 8 — Embedding surface.** Not started.
`engine` API polish, `capi` + cbindgen header + a minimal Python ctypes example, CLI
completion, embedder documentation, versioning policy. *Exit: third-party-consumable
artifacts; docs build; semver-checked public API.*

**Phase 9 — CDP.** Not started.
Transport + domain subset (§5.13), console/network event plumbing, screenshots/PDF via
CDP, cookie domain methods. *Exit: Puppeteer and Playwright basic suites (connect, goto,
evaluate, screenshot, pdf, cookies) green in CI.*

**Phase 10 — Hardening (ongoing).** Not started as a phase, though pieces of its scope
(`IntersectionObserver`/`ResizeObserver`) already landed early — see above.
WPT expansion, fuzz corpus growth (§9 — no `fuzz/` crate exists yet), memory-leak audits
(wrapper pins, detached trees), performance benchmarking + optimization, security review
against §8, input events via CDP `Input`. *Exit: declared v1.*

Cross-cutting rules: WPT expectations updated in the same PR as behavior changes; every
bug fix lands with a regression test; benchmarks run on every merge to main.

---

## 11. Risks & Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| stylo integration complexity (API churn, build weight, trait-impl subtleties) | Phase 4 slip | Pin stylo revision; treat Blitz's `blitz-dom` as the reference implementation; upgrade stylo on a schedule, not continuously. |
| taffy gaps: no floats; inline-level edge cases at the taffy↔parley seam | Layout wrongness on legacy pages | Declare floats out of scope for v1 (§12); invest in the IFC seam early in Phase 5 (it is the highest-unknown item); reftests gate the seam. |
| QuickJS-NG performance ceiling on JS-heavy SPAs | Slow automation on some sites | Engine trait keeps V8 viable; benchmark-driven decision to promote the V8 feature to a supported configuration. |
| Cross-heap cycle leaks (node ↔ JS closure) | Memory growth on long-lived pages | Per-navigation teardown reclaims everything; document the model; pin-count audits in Phase 10; revisit with engine GC-tracing hooks if real workloads demand it. |
| WebIDL codegen scope creep | Phase 2 slip | Generate only what the supported IDL surface needs; hand-write genuinely one-off interfaces; codegen exists to enforce uniformity, not to be complete. |
| CDP compatibility rabbit hole (clients depend on undocumented behaviors) | Phase 9 slip | Scope = make Puppeteer/Playwright basic suites pass, nothing more; expectations tracked like WPT. |
| parley maturity for complex scripts (Indic, CJK line-breaking niceties) | Text fidelity gaps | Acceptable for v1 target use cases; track upstream; reftest per-script samples to know exactly where we stand. |
| Team browser-internals learning curve | Underestimated everywhere | Specs are the design docs (DOM, HTML, Fetch, CSSOM); WPT-first workflow converts unknowns into failing tests early. |

---

## 12. Deliberate v1 Limitations

Stated up front, per P6 (absent beats fake). This is the *original* v1 scope statement;
several of these were later closed (see §10's "Landed beyond the original plan") and
ADR-0009 through ADR-0018 record their own, narrower v1 limits where they apply — check
there, and [`status.md`](status.md), for the current line rather than trusting
this list alone as it ages:

- **`document.write` outside an active parser script**: no-op with a console warning.
  Calls made *by* a parser-inserted script are supported: the markup is prepended to the
  parser's unconsumed input and participates in ordinary tree construction and script
  scheduling, bounded by a per-document write budget. What stays unsupported is the
  destructive `document.open` path a write after parsing would take; that is reported
  rather than emulated.
- **Floats** (`float`/`clear`): laid out, but **inline content does not wrap around them**.
  taffy's `float_layout` is enabled, so a float is taken out of flow, block boxes overlap it
  as they should, and `clear` resolves against it. What is missing is float-aware inline
  layout: a line box next to a float keeps its full width instead of shortening, so text
  paints over the float rather than beside it. That last piece is the top post-v1 layout item.
- **Tables**: fixed-layout approximation via grid mapping in v1; full CSS table layout
  post-v1.
- ~~Iframes: not loaded (element exists, no nested browsing context)~~ — **partly landed**
  (ADR-0035); struck through rather than deleted so this section stays a legible record of
  what the original plan punted on. An `<iframe>` now owns a real nested browsing context:
  its own document in the shared arena, its own style and layout engines, its own realm.
  `src`/`srcdoc` load, scripts inside a frame run in that frame's realm, the element is a
  replaced box, the frame's content is spliced into screenshots and PDFs, the window
  family and `postMessage` cross the boundary, and the protocol reports the real frame
  tree — so `page.frames()`, `frame.evaluate()` and `frameLocator()` all work in both
  drivers. Input, hit testing, `:hover` and focus cross the boundary; *events* do not, as
  the spec says. Named targets, `window.name` and the `allow-scripts`/`allow-same-origin`
  slice of `sandbox` are in. Still out: joint session history (a frame's is replace-only),
  indexed `window[0]`, `<object>`/`<embed>`/`<frameset>`, and out-of-process frames.
- **Workers, service workers, WASM** (under QuickJS): absent. WASM arrives with the V8
  configuration if ever needed.
- ~~Shadow DOM: absent in v1~~ — **landed** (ADR-0010); struck through rather than
  deleted so this section stays a legible record of what the original plan punted on.
- **No speculative HTML parsing beyond prefetch scanning**; no `<link rel=preload>`
  processing in v1.
- **Scrolling**: offsets are real and clamped, but no scrollbars are painted and no
  smooth-scroll/scroll-event timing model beyond dispatching `scroll` tasks.
- **CORS preflight**: Phase 10; until then non-simple cross-origin fetch/XHR is rejected
  under spec-default policy (or allowed under the explicit relaxed policy).

---

## 13. Future Directions (post-v1)

- GPU backend (vello) + windowed embedder → GUI frontend path.
- `<canvas>` 2D over tiny-skia, float-capable block layout, full tables.
  (`IntersectionObserver`/`ResizeObserver` and Shadow DOM, both listed here originally,
  landed early — ADR-0010, ADR-0011 — well ahead of the GPU/embedding-surface work they
  were grouped with.)
- Shadow DOM completeness beyond v1: event retargeting, declarative Shadow DOM,
  `slotchange`, `:host(...)`, `exportparts` (ADR-0010's own limits list).
- Disk HTTP cache; HTTP/3 (quiche/quinn) behind the same connector policy.
- Accessibility tree export (useful for automation selectors, not just a11y).
- Deterministic-replay mode (seeded scheduler + recorded network) for flake-free CI.
- V8 configuration promoted to tier-1 if SPA workloads demand it; WASM follows.
