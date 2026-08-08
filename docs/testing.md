# Conformance testing (`cargo xtask`)

`xtask` is aliased in `.cargo/config.toml`. Its arg parser is hand-rolled —
flag syntax is exact (`--filter <substr>` as two argv items).

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

`puppeteer` and `playwright` both need a Node toolchain and install their own
directory's pinned driver (`tests/automation`'s `puppeteer-core`,
`tests/playwright`'s `playwright-core`) on first run. Each starts the CDP
endpoint **in process** and serves its fixtures from loopback, so CI still
touches no network. Everything not driver-specific is shared in
`xtask/src/nodeharness.rs`, so a fix to the harness contract cannot apply to
one runner and not the other.

`--filter` narrows the *comparison*, not the run: the whole suite executes and
only the matching checks are held against expectations. That is why a filtered
run still reports the full pass count, and why `--update` refuses `--filter`.

**Run the two driver suites apart from the WPT sweep.** Both drive one endpoint
with generous connect/`newPage` budgets, and WPT's 10-way-parallel sweep on the
same machine is enough load to miss them. In CI they are separate jobs for
exactly this reason.

`tests/wpt/vendor/` and `tests/html5lib-tests/` are committed, so a fresh
clone needs no fetch. `fetch-wpt` / `fetch-html5lib` exist only to bump the
pinned upstream revisions:

```sh
cargo xtask fetch-wpt          # vendor the pinned WPT subset
cargo xtask fetch-html5lib     # re-vendor the html5lib-tests tree-construction suite
```

## The expectation files are a two-sided contract, not a suppression list

All four fail CI on regressions **and on unexpected passes and stale
entries**:

- `tests/wpt/expectations.tsv` — only non-PASS outcomes are listed; absent
  means expected PASS. Regenerate with `cargo xtask wpt --update` (it
  refuses `--filter`, since an update rewrites the whole file, and refuses to
  write if the run had any HANG/CRASH — re-run until clean).
- `tests/html5lib-expectations.txt` — enforced by a plain `#[test]` in
  `crates/dom/tests/html5lib.rs`, not by xtask. Hand-delete the lines you
  fixed.
- `tests/automation/expectations.tsv` — one `name<TAB>FAIL` line per Puppeteer
  check expected to fail. Regenerate with `cargo xtask puppeteer --update`,
  which refuses `--filter` for the same reason `wpt` does: an update rewrites
  the whole file, so a filtered run would delete every entry it did not see.
- `tests/playwright/expectations.tsv` — the same, for
  `cargo xtask playwright --update`. **Currently empty**: every check passes
  (ADR-0034), so any line appearing here is a regression that was written down
  rather than fixed.

So **fixing a bug breaks CI until you update the expectation**. The
expectation edit lands in the same commit as the behavior change; diff the
regenerated TSV and confirm every line that vanished is one you meant to
fix.

## Goldens and reftests

**Goldens** are byte-compared display-list JSON; on mismatch, actual output
lands in `target/golden-out/`.

**Reftests** have no `--update` — the expectation is a hand-written
`-ref.html`; on failure inspect
`target/reftest-out/{name}-{test,ref,diff}.png` and either fix the code or
edit the reference.

## Determinism

The WPT **Ahem** font is bundled (`crates/layout/assets/Ahem.ttf`) and
registered unconditionally — every glyph is a 1em square, so `font: 100px
Ahem` + `"AAA"` is exactly 300px on every platform. The golden and reftest
runners additionally call `disable_system_fonts()` (a process-wide runtime
latch, since Cargo feature unification makes the `system_fonts` feature
impossible to turn off per-runner), which makes Ahem back every generic
family. `crates/layout/tests/deterministic_fonts.rs` guards that latch and
must stay in its own test binary.

## Try the engine on a page

```sh
cargo run -p oxidepage-cli -- eval page.html "document.querySelectorAll('p').length"
```

See [`../README.md`](../README.md) for the full CLI reference.
