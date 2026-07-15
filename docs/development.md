# Development

## Prerequisites

Building the style engine (Servo's `stylo`) requires **`python3`** on `PATH`
— `stylo`'s `build.rs` runs a mako code generator. No other native toolchain
is needed.

## Build & test

```sh
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

# The optional paint decoders are off by default; CI builds and tests them separately.
cargo clippy -p oxidepage-paint --all-targets --features svg,webp -- -D warnings
cargo test -p oxidepage-paint --features svg,webp
```

Single tests (integration binaries are named by file stem):

```sh
cargo test -p oxidepage-layout                                   # one crate
cargo test -p oxidepage-dom --test html5lib                      # one test binary
cargo test -p oxidepage-page --test geometry -- --exact <name>   # one test fn
cargo test --workspace shadow                                    # substring across all binaries
```

`crates/layout/tests/profile_incremental.rs` is `#[ignore]`d (ad-hoc
profiler): run it with `-- --ignored --nocapture`.

Benchmarks (criterion, not run in CI but compiled by `--all-targets`):

```sh
cargo bench -p oxidepage-layout --bench reflow
cargo bench -p oxidepage-page --bench geometry_rmw
```

## Size-optimized binary

```sh
cargo build --profile min-size -p oxidepage-cli   # -> target/min-size/oxidepage
```

13.6 MiB vs. release's 19.6 MiB, for well under 1% wall-clock cost; build
time roughly triples. The `min-size` profile keeps `opt-level = 3` and
`panic = "unwind"` **on purpose**, and the reasons are load-bearing — read
the comment above the profile in `Cargo.toml` before touching it. In short: a
whole-binary `opt-level = "s"` measured **2x slower** end-to-end (the engine
is compute-bound in raster/shaping/cascade/JS), and `panic = "abort"` would
defeat `layout::webfont`'s `catch_unwind` trust boundary, turning a hostile
web font into a remote process kill. Per-crate `opt-level = "s"` entries are
each measured, not guessed.

## Regenerating WebIDL bindings

After editing `crates/idl/webidl/*.webidl`:

```sh
cargo xtask codegen            # --check verifies freshness (runs in CI)
```

`crates/bindings/src/generated.rs` is `@generated` and must never be
hand-edited; see "Adding a DOM interface or method" in the architecture
notes below for the full workflow.

## Workspace layout

```
crates/
├── base          # ids, geometry primitives, error types, atom re-exports
├── dom           # arena DOM, TreeSink, events, MutationObserver, serializer,
│                 # selectors integration (querySelector*)
├── js            # JsEngine/JsRealm/JsScope traits + QuickJS-NG backend
├── idl           # WebIDL sources + codegen (weedle2 → bindings glue)
├── bindings      # wrapper cache & pins, generated + hand-written DOM glue,
│                 # JS event dispatch, observers, console, globals
├── net           # fetch stack: SSRF connector, HTTP(S) client, cookies,
│                 # cache, redirect/referrer pipeline, NetService bridge
├── page          # event loop, timers, lifecycle, navigation, rAF, Page API
├── cli           # `oxidepage eval | dump-layout | dump-display-list | render`
├── style         # stylo integration: stylesheet set, cascade, CSSOM ops
├── layout        # box tree, taffy driver, parley IFCs, geometry, image store
├── paint         # box tree → display list (backgrounds, borders, text, images)
├── raster-skia   # display list → tiny-skia CPU raster → RGBA / PNG
├── export-pdf    # display list → single-page PDF (pdf-writer)
├── raster-vello, engine, capi, cdp        # stubs for later phases
xtask/            # cargo xtask: vendoring, codegen, WPT / golden / reftest runners
tests/
├── html5lib-tests/   # vendored html5lib tree-construction suite
├── wpt/              # vendored WPT subsets + expectations.tsv
├── goldens/          # display-list JSON goldens (cargo xtask golden)
└── reftests/         # Ahem pixel-compare reftests (cargo xtask reftest)
docs/adr/         # architecture decision records
```

`page` is the only crate that sees the whole stack — it is the natural entry
point for embedding the engine as a Rust library. `bindings` deliberately
does not depend on `paint`/`raster-skia`/`page`; the render cache lives on
`Page`, not on `PageState`. `capi`, `cdp`, `engine`, and `raster-vello` are
documented stubs for later phases.

## Adding a DOM interface or method

`crates/bindings/src/generated.rs` is `@generated` — **never edit it**.
Hand-written implementations live one-module-per-interface in
`crates/bindings/src/imp/`, where function names are snake-cased member names
(`set_*` for setters, `constructor` for IDL constructors). The generated glue
*calls* these, so an IDL change surfaces as a compile error in `imp/` — that
is the drift protection.

1. Add the interface/member to the right `crates/idl/webidl/*.webidl`.
2. For a *new* interface, add it to `NODE_INTERFACES` and/or the
   `this_unwrap` match in `crates/idl/src/lib.rs`. Every registered interface
   needs a `this`-unwrap, even with zero members. An unsupported IDL
   construct is a **build-time error, not a silent gap**.
3. If node-backed, map tag → interface in `html_interface_for`
   (`crates/bindings/src/cx.rs`).
4. `cargo xtask codegen` (CI gates freshness with `--check`).
5. Implement the now-missing `imp::<module>::<fn>`; the compiler dictates the
   exact signature.

JS-side helpers (the `WeakMap` wrapper cache, proxy traps) live in
`crates/bindings/src/bootstrap.js`, `include_str!`d and evaluated at realm
install.

## Conventions

- **Absent beats fake.** APIs we do not implement are **not installed**.
  Feature detection must work. No always-failing stubs, no silent no-ops —
  deliberate, documented exceptions only.
- **Conformance is automated.** Correctness is measured against WPT, not
  against recollection of the spec — see [`testing.md`](testing.md).
- Deps in `[workspace.dependencies]` (root `Cargo.toml`) carry comments
  explaining every pinned version (stylo ↔ selectors/cssparser, stylo ↔
  stylo_taffy ↔ taffy move in lockstep; rquickjs is pinned for the
  nested-module `meta()` path). Read the comment before bumping.
- Testing has no shared helper crate: tests that need HTTP hand-roll a
  loopback server on `127.0.0.1:0`. Net tests reach it via
  `ResourcePolicy::permissive_localhost()`; the default policy blocks
  private hosts, and **CI never touches the real internet**.

## Further reading

- [`rust-engine-design.md`](rust-engine-design.md) — the architecture
  baseline: design principles, pipeline, threading model, security posture,
  phase plan.
- [`adr/`](adr/) — decisions and deviations recorded per phase. **ADRs win
  over the design document where they conflict.**
- [`status.md`](status.md) — what is implemented today, phase by phase.
