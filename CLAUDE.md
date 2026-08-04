# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

OxidePage is a modular, embeddable **headless web engine** in Rust, assembled from production-grade components (html5ever, stylo, taffy, parley, QuickJS-NG, tiny-skia). It targets headless automation and offscreen rendering (PNG, PDF).

## Authority of documents

1. `docs/rust-engine-design.md` — the architecture baseline (design principles **P1–P7** in §2, pipeline §4, threading §7, security §8, phase plan §10, deliberate v1 limits §12).
2. `docs/adr/` — ADRs record deviations from and refinements of the baseline. **ADRs win over the design document where they conflict.**
3. `docs/status.md` — current phase status; keep it in sync with behavior changes. `README.md` itself is user-facing (what the project does, install, CLI/library usage) — don't put implementation status or dev workflow there.

Write a new ADR (copy `docs/adr/0000-template.md`) for any change to crate boundaries, public API shape, dependency selection, security posture, or a design principle. ADR-0003…0027 cover the phases already landed and are the fastest way to learn *why* a subsystem looks the way it does.

## Build & test

**Prerequisite:** `python3` must be on `PATH` — stylo's `build.rs` runs a mako code generator. No other native toolchain is needed.

```sh
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

# The optional paint decoders are off by default; CI builds and tests them separately.
cargo clippy -p oxidepage-paint --all-targets --features svg,webp -- -D warnings
cargo test -p oxidepage-paint --features svg,webp

# Size-optimized binary (14.4 MiB vs release's 20.6 MiB, <1% slower):
cargo build --profile min-size -p oxidepage-cli   # -> target/min-size/oxidepage
```

The `min-size` profile keeps `opt-level = 3` and `panic = "unwind"` **on purpose**, and the reasons are load-bearing — read the comment above it in `Cargo.toml` before touching it. In short: a whole-binary `opt-level = "s"` measured **2x slower** end-to-end (the engine is compute-bound in raster/shaping/cascade/JS), and `panic = "abort"` would defeat `layout::webfont`'s `catch_unwind` trust boundary, turning a hostile web font into a remote process kill. Per-crate `opt-level = "s"` entries are each measured, not guessed; `psl` is deliberately absent because "s" changes it by zero bytes (it is a compiled data table).

Single tests (integration binaries are named by file stem):

```sh
cargo test -p oxidepage-layout                                   # one crate
cargo test -p oxidepage-dom --test html5lib                      # one test binary
cargo test -p oxidepage-page --test geometry -- --exact <name>   # one test fn
cargo test --workspace shadow                                    # substring across all binaries
```

`crates/layout/tests/profile_incremental.rs` is `#[ignore]`d (ad-hoc profiler): run with `-- --ignored --nocapture`.

Benchmarks (criterion, not run in CI but compiled by `--all-targets`):

```sh
cargo bench -p oxidepage-layout --bench reflow
cargo bench -p oxidepage-page --bench geometry_rmw
```

## Conformance runners (`cargo xtask`)

`xtask` is aliased in `.cargo/config.toml`. Its arg parser is hand-rolled — flag syntax is exact (`--filter <substr>` as two argv items).

```sh
cargo xtask codegen [--check]        # regenerate crates/bindings/src/generated.rs from WebIDL
cargo run --release -p xtask -- wpt  # WPT subsets; ALWAYS --release (debug takes ~11 min)
cargo xtask wpt --filter <substr>    # substring of the full test path
cargo xtask wpt-single tests/wpt/vendor/dom/nodes/Node-appendChild.html
cargo xtask wpt --update             # rebaseline expectations.tsv
cargo xtask golden [--update] [--filter <stem>]   # display-list JSON goldens
cargo xtask reftest [--filter <stem>]             # Ahem pixel reftests
cargo xtask puppeteer [--update] [--filter <substr>]  # real Puppeteer over the CDP endpoint
cargo xtask playwright [--update] [--filter <substr>] # real Playwright over the CDP endpoint
```

`puppeteer` and `playwright` both need a Node toolchain and install their
directory's pinned driver (`puppeteer-core`, `playwright-core`) on first run.
Each starts the endpoint **in process** and serves fixtures from loopback, so CI
still touches no network. Everything not driver-specific is shared in
`xtask/src/nodeharness.rs`, so a fix to the harness contract cannot apply to one
runner and not the other.

`tests/wpt/vendor/` and `tests/html5lib-tests/` are committed, so a fresh clone needs no fetch. `fetch-wpt` / `fetch-html5lib` exist only to bump the pinned upstream revisions.

**The expectation files are a two-sided contract, not a suppression list.** Both runners fail CI on regressions **and on unexpected passes and stale entries**:

- `tests/wpt/expectations.tsv` — only non-PASS outcomes are listed; absent means expected PASS. Regenerate with `cargo xtask wpt --update` (it refuses `--filter`, since an update rewrites the whole file, and refuses to write if the run had any HANG/CRASH — re-run until clean).
- `tests/html5lib-expectations.txt` — enforced by a plain `#[test]` in `crates/dom/tests/html5lib.rs`, not by xtask. Hand-delete the lines you fixed.
- `tests/automation/expectations.tsv` — one `name<TAB>FAIL` line per Puppeteer check that is expected to fail. Regenerate with `cargo xtask puppeteer --update` (it refuses `--filter`, for the same reason `wpt` does).
- `tests/playwright/expectations.tsv` — the same, for `cargo xtask playwright --update`.

So **fixing a bug breaks CI until you update the expectation**. The expectation edit lands in the same commit as the behavior change; diff the regenerated TSV and confirm every line that vanished is one you meant to fix.

**Goldens** are byte-compared display-list JSON; on mismatch, actual output lands in `target/golden-out/`. **Reftests** have no `--update` — the expectation is a hand-written `-ref.html`; on failure inspect `target/reftest-out/{name}-{test,ref,diff}.png` and either fix the code or edit the reference.

**Determinism:** the WPT **Ahem** font is bundled (`crates/layout/assets/Ahem.ttf`) and registered unconditionally — every glyph is a 1em square, so `font: 100px Ahem` + `"AAA"` is exactly 300px on every platform. The golden and reftest runners additionally call `disable_system_fonts()` (a process-wide runtime latch, since Cargo feature unification makes the `system_fonts` feature impossible to turn off per-runner), which makes Ahem back every generic family. `crates/layout/tests/deterministic_fonts.rs` guards that latch and must stay in its own test binary.

## CLI

```sh
cargo run -p oxidepage-cli -- eval <file.html | http(s)://URL> [expr] [--viewport WxH] [--settle-ms N]
cargo run -p oxidepage-cli -- dump <file|url> [--format layout|display-list] [-o out]
cargo run -p oxidepage-cli -- render <file|url> -o out.{png,jpg,pdf,html} [--dpr N] [--full-page] [--paper A4]
```

```sh
cargo run -p oxidepage-cli -- serve [--port N] [--viewport WxH] [--allow-private]
```

`--allow-private` is needed to hit loopback/private hosts (SSRF filter is on by default).

`serve` starts the CDP endpoint and prints its WebSocket URL on **stdout** (the banner goes to stderr). Five defences, all structural: it binds `127.0.0.1` only; every request must carry a loopback `Host` (without that check a hostile page rebinds its own domain to `127.0.0.1` and drives the endpoint through the user's browser); a request carrying an `Origin` is refused, because a browser applies no CORS to `new WebSocket` and such a request has a genuinely loopback `Host`; `/json/new` takes `PUT` only, since `GET`/`POST` are CORS-simple and an `<img src=…>` would otherwise let any page open and navigate a target; and the WebSocket path carries a 128-bit CSPRNG token. The token is **not** secret from anything that can reach the port and read a reply — `/json/version` publishes it, as Chrome's does, because that is how `puppeteer.connect({ browserURL })` finds the socket. The SSRF filter protects the content the engine loads, not the operator: reaching the port is equivalent to owning the process.

## Architecture

### Layering (strictly acyclic)

```
base ──┬─ dom ── style ── layout ── paint ─┬─ raster-skia ─┐
       │                                   └─ export-pdf ──┤
       ├─ net ─────────────────────────────────────────────┤
js ────┴───────────── bindings ─────────────────────────────┴── page ─┬─ engine ── cdp ─┬─ cli
                                                                      └─────────────────┘
idl (build-time only: consumed by xtask codegen, not by any runtime crate)
```

- **`idl` is not a dependency of `bindings`.** It is a codegen library driven only by `cargo xtask codegen`; its output is checked in as `crates/bindings/src/generated.rs`.
- **`page` is the only crate that sees the whole stack.** `bindings` deliberately does not depend on `paint`/`raster`/`page` — the render cache lives on `Page`, not on `PageState`.
- **A world is a whole `Runtime`, and `page` owns the table of them** (ADR-0033). `WorldTable` lives in `crates/page`; `bindings` reaches another world only through the `WorldEnter` trait it defines, held as a **`Weak`** on `PageShared` so the `Page → WorldTable → realm → WorldState → PageShared` chain stays acyclic. Entering a world already on the stack is refused, not queued: `Context::with` is a `RefCell::borrow_mut`, so re-entry would panic.
- **`engine` is above `page`, and `cli` does not go through it** (ADR-0027). `Browser` → `BrowserContext` → `PageHandle` are `Send + Sync` façades over one OS thread per `Page` — `Page` is permanently `!Send` (rquickjs without `parallel`, stylo's thread-locals), so only messages cross. The edge points one way and stays that way: `page` learns nothing about browsers or protocols because a command is an opaque `Box<dyn FnOnce(&Page) + Send>`. Adding a `page → engine` dependency is how that inverts.
- **`cdp` is above `engine`, and only `oxidepage serve` reaches it** (ADR-0030). It owns the WebSocket, the target registry and the command lanes; nothing below it knows the protocol exists. Every other CLI subcommand still drives one `Page` synchronously and depends on neither `engine` nor `cdp`.
- `capi` and `raster-vello` are documented **stubs** for later phases.

### Pipeline

`Page::navigate` → `NetService::fetch_blocking` → byte decode (BOM > HTTP charset > `<meta charset>`) → **streaming parse loop**: `Parser::run()` returns `ParseSignal::Script(node)` at each suspension point, where the loop drains style updates, runs the script, then resumes.

`Page::flush_layout()` is the single funnel into layout: `drain_style_updates()` then `LayoutEngine::reflow()`, which early-outs on a `ReflowStamp`, runs the stylo restyle traversal, then either incrementally patches the box tree or rebuilds it, then runs taffy.

`build_display_list()` walks the box tree in stacking-context order. The `DisplayList` is flat, backend-neutral, `Send` and JSON-dumpable — raster (`tiny-skia`) and PDF (`pdf-writer`) are dumb consumers of the same list (**P5**).

The cached display list is keyed on a `PaintStamp` (dom/style/element-scroll/images/fonts versions) and is built **unscrolled** — document scroll is applied at *raster* time, so `position: fixed` stays pinned and the cache survives scrolling. Full-page lists are never cached (the stamp can't distinguish the two paint origins).

### Invariants you will silently break if unaware

**Generation-checked ids.** `NodeId` is `{u32 index, NonZeroU32 generation}`. `Arena::free` bumps the generation; a slot whose generation would wrap is retired, not recycled. `Arena::node`/`node_mut` **panic** on stale ids — use them only where liveness is established, else `get`/`get_mut`. Any `NodeId` stored across a task boundary is a snapshot and **must be re-validated at the drain boundary** (`StyleUpdate`, image updates, `MutationRecord.target`, `Event.target`).

**One invalidation code path** (`crates/dom/src/tree.rs`). Every mutation goes through `insert_internal` / `remove_internal` / the attribute and character-data primitives, which mutate, queue MutationObserver records, and run the invalidation hook (dirty bits + stylo `RestyleHint`/`ServoElementSnapshot`). Public spec algorithms validate and *delegate*. **Never mutate node fields directly** — invalidation, `propagate_connectedness`, `note_stylo_restyle`, and `snapshot_element` would be skipped silently. Downstream consumers key on `style_version` / `structure_version` / `id_version`.

**Wrapper cache identity + pin contract** (design §5.3, ADR-0033, `crates/bindings/src/cx.rs`). **All of this is per world.** A world is a whole `rquickjs::Runtime` — one global, one set of prototypes, one wrapper cache, one slab, one listener registry, one remote-object store — over one shared DOM, so a node has *N* wrappers and no `JsValue` can cross (`Persistent::restore` compares runtime pointers, which is what makes a leaked wrapper a typed error rather than silent corruption). Within a world: the cache is keyed by arena *index*, so `node_to_js` generation-checks both the incoming id and the cache hit's payload — otherwise a stale id returns the wrapper of an unrelated node now in that slot. Creating a wrapper **pins** the node; `DomTree::pin` is a refcount, so *N* worlds hold *N* pins and the node frees only when the last one goes. The cache is weak, and GC finalization flows back through `Page::process_finalized` → `dom.unpin(id)` → `free_detached_tree_if_unpinned`; `take_finalized` is per realm, so a world's slab keys can only ever reach its own state. Freeing is refused while the parser is active or mutation records are pending. A wrapper is not the only pin holder: **pending activity pins too** — a queued or in-flight `<img>` load holds one from `push_image_update` / `start_image_load` until the drain and `notify_image_waiters` release it (ADR-0028), because the preload idiom keeps no JS reference to the element. **Exception:** upgraded custom elements are held strongly in `WorldState::custom_wrappers`, main world only. Connected-wrapper retention (ADR-0018) is a page-level **log** with a per-world cursor, because the DOM's own queue is destructive and the first world to drain would starve the rest. `WorldState` is recovered from the JS scope, never captured — that is what makes JS→Rust cycles structurally impossible, and it is why `WorldTable::teardown` releases every world's values before freeing any runtime: a `Persistent` outliving its `Runtime` aborts the process.

**One transform, resolved once** (ADR-0026). `layout::transform::resolve` is the only place a CSS `transform`/`translate`/`rotate`/`scale` becomes a matrix, and it lives in `layout` — not `paint` — because geometry and hit testing need it and `paint` depends on `layout`, not the reverse. The matrix it returns is in the space of the `border_box` handed in (origin baked), which is what lets paint pass an absolute box and layout a local one. A post-layout pass caches the **local** matrix on `LayoutBox::transform` (geometry has no access to computed styles); `geometry::absolute_frame` composes the chain, `hit_box` inverts it. Writing a second resolver is how paint and geometry drift apart — the same reason `multicol::map_flow_point` exists. Note the deliberate carve-out: `offset*`, `client*` and `scrollWidth`/`scrollHeight` stay **untransformed** per CSSOM-View, and a WPT file pins that.

**The flat tree is the one authoritative tree.** `DomTree::flat_tree_children`/`flat_tree_parent` implement shadow-DOM slot projection, and **both** consumers — stylo's restyle traversal (`dom/src/stylo.rs`) and box-tree construction (`layout/src/construct.rs`) — use it and nothing else. A new traversal written against `children()` silently desyncs style from layout under shadow DOM.

**There are many documents, but only one *rendered* one** (ADR-0017). `new Document()` / `DOMParser` / `createHTMLDocument` create real Document nodes. They are inert **structurally**, not defensively: `propagate_connectedness` grants `NodeFlags::IS_CONNECTED` only under `self.document`, and style, layout, resource loading, custom-element upgrades, the `getElementById` index and event bubbling to the `Window` all gate on that flag. **Image loading is the one exception** (ADR-0028): it gates on `node_document(el) == dom.document()`, because `new Image().src = …` is detached by construction and HTML loads it anyway — a `DOMParser` document still loads nothing, which is the rule the flag was standing in for. So keep `IS_CONNECTED` meaning "in the rendered document" — the *spec's* `isConnected` is `DomTree::is_spec_connected`, and only the JS getter uses it. A document member that reflects the browsing context (`defaultView`, `currentScript`, `styleSheets`, `readyState`, anything that flushes layout) must check `this == dom.document()`; `imp/document.rs` has `is_page_document` for exactly that. **A `document::` member takes a *document*** — `scrollParent` once passed an Element and got away with it only because the parameter was ignored.

**A pinned node pins its node document** (`DomTree::pin`/`unpin`/`adopt`). A node created by `doc2.createElement()` and never inserted is its *own* detached root, so it is not in doc2's subtree and `subtree_has_pins(doc2)` cannot see it — without the owner pin, GC of the doc2 wrapper would free doc2 and leave `el.ownerDocument` naming a freed slot. `pins[doc]` therefore counts the document's own wrappers **plus** one per pinned node it owns, and adoption *moves* the pin. `Node.owner` is `None` **iff** the node is a Document; `insert_internal` re-owns the inserted subtree (the spec's adopt-on-insert), which is why the parser, cloning and grafting get adoption for free.

**`CDATASection` is a Text node for every rule** — hierarchy validity, `:empty`, whitespace, layout. Test with `Node::is_text` / `is_text_kind`, never `NodeKind::Text` alone; the compiler will demand a new `match` arm and happily accept the wrong (permissive) answer. `normalize()` is the one deliberate exception (it merges *exclusive* Text nodes, which a CDATASection is not).

**Reflow must never re-enter JS** — callers hold `RefCell` borrows on dom/style/layout.

**`unsafe` is denied workspace-wide.** `crates/dom/src/stylo.rs` is the single `#![allow(unsafe_code)]` module (stylo mutates through `&self`; soundness rests on interior mutability plus stylo's exclusive per-node access). A `NodeRef` is a pointer-sized handle valid only inside an `enter_active_tree` scope.

**Navigation:** the document node is always arena slot `(0, gen 1)`, so the JS `document` wrapper survives navigation. Every *other* old id goes stale only because the replacement arena is seeded above the outgoing one's generation high-water mark (`DomTree::with_generation_base`) — a plain fresh arena would re-issue the same generations, and an id the old document handed to script would silently **alias** an unrelated new node instead of dying. `RenderState::reset()` must be called on navigation, or the fresh engine's all-zero stamp matches the stale cached list.

### Event loop (`crates/page/src/lib.rs`)

`run_until_stalled_until(deadline)` drains task sources in a fixed order: finalized wrappers → queued navigation → **embedder commands** → style updates → script updates → custom-element reactions → image updates → visible/background image loads → inline-SVG raster → font-face loads → sibling `storage` events → scroll events → observer delivery (ResizeObserver/IntersectionObserver — deliberately *outside* "update the rendering", per ADR-0011) → net events → one due timer → a rendering opportunity on a 16 ms cadence.

`Page::settle(budget)` is the **settle budget**: run to stall, return iff no timer, no pending rAF, and no in-flight net. Otherwise it does exactly **one blocking wait** — `wait_for_work(min(next_timer, next_render, budget_end))`, a `crossbeam::Select` over the net receiver and the embedder's command port. That single call unifies the async net thread, the timer heap and a driver's commands with the synchronous page thread (ADR-0004, ADR-0027 D4): no busy-wait, no polling. It is the *only* park — `await_subresources` and `await_pending_stylesheets` call the same helper, so the count of blocking waits per iteration stays one. **A disconnected `Receiver` is permanently ready in a `Select`**, so `wait_for_work` returns `false` on command-port disconnect and the caller must stop looping; re-registering it converts the one park into a 100%-CPU spin visible only as a CPU graph.

**The command port is a task source, and ordinary jobs obey the navigation guard.** A `PageJob` arriving mid-wait runs immediately only if `!in_job && !suspended && !navigating && !state.parsing()`; otherwise it parks in `pending_jobs` and drains at the top of the loop, after the navigation drain (which invalidates every node id a job might hold) and before the page's own sources (a job is the driver's turn and must not be starved). This is the same guard ADR-0022 gave script navigation, for the same reason: `await_subresources` and `await_pending_stylesheets` re-enter the loop from *inside* `load_document`, holding `RefCell` borrows on dom and style plus live parser handles, so a job that evaluated script there is a deterministic `BorrowMutError`, not a race. **`control` jobs are the exception and run at any wait point** — today `Page::request_close`, `Page::suspend` and `Page::resume`, which is what keeps a page interruptible while it is blocked on a slow load. `PageJob::control` is public, so the bar is a convention you must hold: `Cell`s and channels only. Anything that enters JS, borrows the DOM or flushes layout is a `BorrowMutError` waiting for the right timing. A dialog answer is the related case that is *not* a job at all — it travels on its own rendezvous channel, because a page parked in `run_dialog` services no ordinary job and an answer queued on the command port would sit behind the very block it is meant to release.

`Page::with_cx` is the single entry into JS; it arms the `ScriptBudget` (10 s default, enforced via the engine interrupt callback) at the outermost call, so a task and its microtasks share one deadline. Microtask checkpoints run after every callback into JS.

**MutationObserver delivery is a real microtask, not a checkpoint sweep.** The compound microtask is enqueued onto the *engine's own promise-job queue* the moment a record is queued — from the host-call trampoline (`cx::native`), the one point where the mutating call has released its `dom` borrow and no further JS has run. That is what orders delivery **against** promise reactions: `await Promise.resolve()` later in the same task must not overtake records queued before it. Delivering observers only after `pump_jobs()` had drained the whole job queue inverted that ordering. `microtask_checkpoint`'s trailing `deliver_mutation_observers` remains the fallback for records queued outside JS (the parser).

`NetService` owns a multi-thread tokio runtime *living on the page thread*; requests are spawned and progress comes back as `NetEvent::{Headers, Chunk, Done, Error}` over a crossbeam channel, tagged by `RequestId`. `dispatch_net_event` routes by id through the page-owned maps (async scripts → sheets → images → fonts), falling through to `bindings::deliver_net_event` for script-initiated `fetch`/XHR. It **must** call `finish` on every terminal event or the bookkeeping grows unbounded.

**`data:` is decoded above the scheme gate, not by widening it** (ADR-0029). `fetch_inner` returns early for `data:` beside `file://`, so every consumer — scripts of all four flavours, modules, sheets, `@import`, images, fonts, `fetch`/XHR — gets it for free and the async ones keep their `NetEvent` timing. Do **not** add `data` to `ResourcePolicy::allowed_schemes`: the early return is deliberately *outside* the redirect loop, which re-checks the gate per hop, and that is the only thing keeping an `http:` → `data:` redirect a network error. `net::data::decode` is the one decoder — it percent-decodes *before* base64, per the Fetch data: URL processor, and `page`'s inline image/`@font-face` paths call it rather than rolling their own.

### Adding a DOM interface or method

`crates/bindings/src/generated.rs` is `@generated` — **never edit it**. Hand-written implementations live one-module-per-interface in `crates/bindings/src/imp/`, where function names are snake-cased member names (`set_*` for setters, `constructor` for IDL constructors). The generated glue *calls* these, so an IDL change surfaces as a compile error in `imp/` — that is the drift protection.

1. Add the interface/member to the right `crates/idl/webidl/*.webidl`.
2. For a *new* interface, add it to `NODE_INTERFACES` and/or the `this_unwrap` match in `crates/idl/src/lib.rs`. Every registered interface needs a `this`-unwrap, even with zero members. An unsupported IDL construct is a **build-time error, not a silent gap**.
3. If node-backed, map tag → interface in `html_interface_for` (`crates/bindings/src/cx.rs`).
4. `cargo xtask codegen` (CI gates freshness with `--check`).
5. Implement the now-missing `imp::<module>::<fn>`; the compiler dictates the exact signature.

JS-side helpers (the `WeakMap` wrapper cache, proxy traps) live in `crates/bindings/src/bootstrap.js`, `include_str!`d and evaluated at realm install.

## Conventions

- **P6 "Absent beats fake":** APIs we do not implement are **not installed**. Feature detection must work. No always-failing stubs, no silent no-ops — deliberate, documented exceptions only.
- **P7 "Conformance is automated":** correctness is measured against WPT, not against recollection of the spec.
- Deps in `[workspace.dependencies]` carry comments explaining every `=x.y.z` pin (stylo ↔ selectors/cssparser, stylo ↔ stylo_taffy ↔ taffy move in lockstep; rquickjs is pinned to `=0.12.0` for the nested-module `meta()` path). Read the comment before bumping.
- Testing has no shared helper crate: tests that need HTTP hand-roll a loopback server on `127.0.0.1:0`. Net tests reach it via `ResourcePolicy::permissive_localhost()`; the default policy blocks private hosts, and **CI never touches the real internet**.
