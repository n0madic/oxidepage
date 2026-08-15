# ADR-0037: A layout deadline, with a `catch_unwind` boundary

- Status: accepted
- Date: 2026-08-15
- Builds on: ADR-0006 (layout via taffy + `stylo_taffy`), ADR-0008 D1 (the
  `catch_unwind` trust boundary around a hostile web font), ADR-0015 (no more
  silent failures), ADR-0036 D5 (the `repeat(auto-fill, …)` hole this closes
  the *shape* of), ADR-0027 (one OS thread per `Page`)
- Constrained by: design §2 P6 (absent beats fake), §7 (threading), §8
  (security posture), §12 (deliberate v1 limits)

## Context

The engine renders untrusted content, and until now it had exactly one budget:
`ScriptBudget` (`crates/page/src/lib.rs`), 10 s of wall clock enforced through
QuickJS's interrupt callback. That callback is polled by the JS engine, so the
budget covers **only** the time a task spends in JavaScript. The code already
said so — `crates/engine/src/page.rs` explains an unrelated bounded wait with
"the `ScriptBudget` cannot fire (the block is in Rust)".

Layout is Rust. Nothing polls anything there, and several documents make the
layout pass itself expensive:

- **The hole ADR-0036 D5 left open.** `repeat(auto-fill, …)` resolves at
  *layout* time from the container's used size, so no style-level cap reaches
  it. `width/height: 20000px` with `repeat(auto-fill, 1px)` on both axes is
  411 MB of peak RSS from ~150 bytes of CSS. Past 65 535 repetitions taffy's own
  arithmetic breaks (`explicit_grid.rs:172` overflows a `u16`: a panic in
  debug, a silent wrap to zero tracks in release). ADR-0036 handed the problem
  here verbatim: *"Bounding this needs a boundary around the layout pass
  itself"*.
- Deeply nested intrinsic sizing, pathological float/line interactions and
  fragmentation are bounded by nothing at all.
- A panic *inside* the layout pass — taffy's `u16` guards being the known
  example — unwinds through `reflow` → `flush_layout` → the page thread's job
  and **kills the thread**. The driver sees `-32000 "page thread panicked"` and
  the page is gone.

Under `oxidepage serve` a slow layout is worse than a slow render: `control`
jobs keep the page interruptible, but the render itself never finishes, so the
driver's `Page.captureScreenshot` never returns (short of `EngineError::Timeout`,
where configured).

The goal is to turn both the hang and the panic into a **typed, recoverable
failure of one page**, without touching taffy's pins (ADR-0006 keeps stylo ↔
`stylo_taffy` ↔ taffy in lockstep) or any crate boundary.

Four facts about `LayoutEngine::reflow` shaped the design, each of which breaks
the obvious implementation:

1. **taffy is entered from four places in `reflow`, not one** —
   `taffy::compute_root_layout`, but also `resolve_intrinsic_size_keywords`,
   `marker::place_markers` and `taffy::round_layout` through `RoundTree`. A
   boundary around the first alone misses three passes.
2. **The build snapshot is written *before* the compute pass.** An abort leaves
   it fresh and valid-looking, so the *next* reflow would take the incremental
   path and patch a tree that was never laid out.
3. **`style.take_restyled_nodes()` has already drained the restyle set** by the
   time an abort is possible. The patch path's input is gone, so recovery
   **must** be a full rebuild. That is correctness, not hygiene.
4. **The reflow stamp is written last**, so an abort leaves it stale and
   nothing short-circuits — which is what makes a retry storm possible.

Plus: `taffy_impl` temporarily mutates a child's `min_size`/`size`/`flex_grow`
and restores it on the way out. An unwind skips the restore, leaving boxes
carrying values that are not their style's — one more reason to throw the tree
away rather than repair it.

## Decision

### D1. The budget is thread-local, and armed at two levels

`crates/layout/src/budget.rs` is a deliberate copy of `ScriptBudget`'s shape,
**including its ownership semantics**: `arm(limit)` returns a guard, the
outermost `arm` owns the deadline and a nested one is a no-op, and `Drop`
disarms — so an unwind cannot leak a deadline into unrelated work.
`Duration::MAX` disables the budget entirely; `Duration::ZERO` trips at the
first checkpoint, which is what makes the tests deterministic without a clock.

Thread-local rather than a field, because `checkpoint()` is called from inside
taffy's recursion where no engine reference is in scope — the same reason
`ScriptBudget` hangs off the runtime's interrupt handler rather than off a task.

Two arming levels, and both are load-bearing:

- **`Page::flush_layout`** arms once for the whole frame walk. Fifty iframes
  must not buy fifty deadlines.
- **`LayoutEngine::reflow` arms it itself** from `LayoutEngine::set_budget`.
  This is not redundancy. `reflow` has a *second* production caller —
  `bindings::imp::geometry_support`, where `el.offsetWidth` and
  `getBoundingClientRect()` land — and it never goes through `Page::flush_layout`.
  Arming only at the flush would leave script-driven layout unbudgeted, i.e.
  exactly the hole this ADR exists to close.

The limit therefore lives on the engine, and `page` is what keeps every engine's
copy current: at page construction, at frame attach, at a navigation's engine
rebuild, on `Page::set_layout_budget`, and once more in the flush loop. Engines
are built by `bindings`, which knows nothing of the page's configuration, so
"sync it where the page can see the frame" is the only place this can live
without inverting a crate dependency.

Stride: a `Cell<u32>` counter in the same thread-local, one `Instant::now()` per
512 checkpoints. The counter starts at zero so the first checkpoint under a
fresh budget always measures.

### D2. Checkpoints at taffy's funnel and at the crate's own loops

| File | Site | Why |
| --- | --- | --- |
| `taffy_impl.rs` | top of `compute_box_layout` | The single funnel: `compute_child_layout`, `compute_block_child_layout` and `TableTreeWrapper` all arrive here |
| `construct.rs` | `Builder::build_box`, the IFC build loop | Deeply nested DOM, before taffy runs at all |
| `multicol.rs` | `resolve_columns` | Post-layout walk, and not a linear one: it calls `break_opportunities` — a subtree walk — per multicol box |
| `transform.rs` | `resolve_transforms` | Post-layout walk with a per-box style lookup |
| `pagination.rs` | the `page_boundaries` loop | Fragmentation checks the same deadline |

`inline.rs` is **not** on this list, though the obvious reading says it should
be: line breaking is ours, not taffy's. But `compute_inline_layout_inner` is
reachable only through `compute_box_layout`, and a checkpoint at its top fires
once per call rather than once per line — so it duplicates the funnel's tick
exactly, halving the effective stride for inline-heavy documents and buying
nothing. Re-adding it needs a checkpoint *inside* the line loop to be worth
anything.

`overflow.rs::resolve_scrollable_overflow` is **deliberately not** instrumented
either, and the rule that separates it from the two walks above is per-box
cost, not "is it a walk": it is a single linear pass that touches each box once
with no nested traversal and no lookup, over a tree whose size the checkpointed
construction pass already bounded.

Tripping raises a typed panic. That is the only way out of taffy's recursion
without forking it — the crate is entered through `compute_root_layout`, which
has no error channel.

The deadline is **not** cleared when it trips, so the remaining frames of a
flush trip immediately instead of each buying a fresh budget. The cost is that
a checkpoint reached while already unwinding would panic during an unwind (an
abort), so no `Drop` impl in `layout` may call `checkpoint`, and none does:
every call site is the top of a loop or of a compute function.

**Not covered:** the inner loop of a single taffy algorithm on a *single* node
— which is precisely ADR-0036 D5's `repeat(auto-fill, …)`. Interrupting that
needs a taffy fork. It is caught only if it panics (D4). This is a residual
hole, stated as one; see Consequences.

### D3. The boundary wraps the whole pass, and argues its own unwind safety

`catch_unwind` covers everything from `resolve_styles` through
`resolve_transforms` — both the construct/patch phase and every taffy entry —
because D2 puts checkpoints in construction and because of fact 1. Unwinding
through the `enter_active_tree` scope is safe: it is an RAII guard and drops
correctly.

ADR-0036 asked that a third boundary "be argued for on the same terms, not added
by analogy", and noted that the two existing ones (`webfont::decode_caught`,
`construct::to_taffy_style_guarded`) each wrap a **pure function of borrowed
input**. This one plainly does not: it mutates the box tree, the build snapshot
and `taffy_impl`'s style latches. The argument is different, and it is this:
**every mutated thing is discarded wholesale on the error path** (D5), so there
is no surviving state through which a half-written value could be observed.

One consequence of adding a boundary *outside* an existing one: the landing pad
in `to_taffy_style_guarded` now re-raises a `LayoutAborted` payload rather than
repairing it into a saturated style. No checkpoint is reachable inside that
closure today; the re-raise keeps that from being a load-bearing accident.

One item the discard argument does **not** cover: `resolve_styles` runs inside
the boundary, and stylo's per-node style data is not ours to throw away —
`discard_tree` does not touch it. That is accepted rather than overlooked,
because of an asymmetry. No checkpoint is polled inside stylo, so a *deadline*
unwind can never originate in `resolve_styles`; only a genuine stylo bug can,
and then per-node styles may be half-updated when the recovery rebuild runs
over them. The alternative was a dead page thread, which is strictly worse. See
Consequences.

### D4. Every payload is caught and classified, including foreign ones

```rust
pub enum LayoutAborted {
    Deadline { limit: Duration },
    EnginePanic(String),
}
```

A foreign panic is **not** resumed. Today it kills the page thread, and ADR-0036
D5 names `explicit_grid.rs:172` as this boundary's target. Classification is by
`payload.downcast::<LayoutAborted>()`; a foreign message is extracted the way
`engine::page::panic_message` does it.

The panic hook is **not** touched, per ADR-0036 D1 (`set_hook`/`take_hook` are
process-global and `Browser` runs one OS thread per `Page`). The price is one
`Box<dyn Any>` line on stderr per abort — at most once per budget period per
page. The informative message is printed by whoever handles the returned error.

### D5. Recovery discards the tree; the stamp makes the retry cheap

```rust
match budget::catch(|| self.reflow_inner(dom, style)) {
    Ok(()) => { self.stamp = Some(stamp); self.aborted_stamp = None; Ok(()) }
    Err(reason) => { self.discard_tree(); self.aborted_stamp = Some(stamp); Err(reason) }
}
```

`discard_tree` replaces the box tree, clears the build snapshot (fact 2) and
clears the success stamp. A rebuild is the only *correct* recovery because the
restyle set was already drained (fact 3), not merely the simplest one.

`aborted_stamp` is the symmetric twin of the existing stamp gate, and it closes
the retry storm fact 4 makes possible: without it, each of the page's many
flushes — and every `offsetWidth` between them — would burn the full budget
again. Any bump of `dom_version` / `style_version` / `viewport` /
`images_version` / `fonts_version` moves the stamp and lifts the block, so
recovery stays automatic with no manual reset anywhere.

A *change of budget* lifts it too, and has to: the cached abort was decided
under the old limit, and a static document never moves its stamp — so without
that, an embedder raising the budget in response to the failure could never
render the page again. `set_budget` clears the gate **only when the limit
actually differs**, because `flush_layout` calls it for every frame on every
flush and an unconditional clear would reinstate the retry storm.

### D6. Geometry getters answer zero, and that is a decision

`bindings::imp::geometry_support` swallows the `Result` explicitly: the abort is
ignored there and the query proceeds against the discarded tree. Public getter
signatures do not change. With no boxes, `offsetWidth` and
`getBoundingClientRect()` give zeros and `getClientRects()` is empty — the same
answers `display: none` produces. Half a rectangle salvaged from an abandoned
compute would be invented data, which P6 forbids.

What that abort does **not** do is reach the embedder directly. It is recorded
on the *engine* (`aborted_stamp`), and the page learns of it only when a later
flush lands on the same stamp and gets the cached error. Script that measures,
reads zeros, and then mutates the DOM — the ordinary measure-then-write idiom —
moves the stamp, so a subsequent flush may well succeed and that turn's zeros go
unreported. This is the same shape as a task killed by the `ScriptBudget`: the
turn is lost, the page is not. Routing it out would mean a new `HostHooks`
method, i.e. a public-trait change in `bindings` for a signal nothing yet
consumes; if an embedder needs per-turn visibility it belongs with per-stage
budget attribution, not here.

### D7. Upward through the existing nested `Result`

`reflow` returns `Result<(), LayoutAborted>`. `Page::flush_layout` absorbs it
into a `Cell` and reports the **first** cause; `Page::take_layout_abort()` reads
and clears.

The walk **continues** past an abort. Stopping at the first one looked right —
the budget is shared, so a frame after a genuine trip dies at its first
checkpoint anyway — but it is wrong for the *cached* abort a fail-fast stamp
returns, which consumes no budget at all: one `<iframe>` that aborted once would
then keep every later frame from being sized or laid out for as long as its own
stamp stood still. So an abort is scoped to the frame that raised it; the page
as a whole still reports failure, because a capture containing one blank frame
is not a successful capture.

**Every flush overwrites the cell in full** — `None` on success. Otherwise a
stale flag gives a false failure: an abort during `dispatch_mouse` that nobody
collected, then a DOM mutation, then a successful flush inside a screenshot, and
`take_layout_abort()` fails that screenshot for a reason that no longer exists.

Paint *peeks* rather than takes: `build_cached_display_list` returns the
previous list when an abort is pending, so a blank picture never displaces a
good one in the cache — and the flag survives for the artifact's caller. That
guarantee is about the **cache**, and the full-page, clipped and PDF lists are
deliberately uncached (the paint stamp cannot tell two paint origins apart), so
those are built blank from the discarded tree. Nothing blank ships through the
CLI or CDP, which check the flag; a direct `page`-crate embedder calling
`print_to_pdf()` gets empty bytes and must read `take_layout_abort()` — which is
what the flag is for.

`LayoutEngine::scroll` deliberately survives `discard_tree`. It is embedder and
user state, not something derived from the box tree, and resetting it would
throw away the scroll position on a transient abort. The cost is that
`Page::layout_metrics` can report a `scroll_y` past the (now viewport-sized)
content extent while an abort is pending — which is why `PageHandle` turns that
call into an error rather than passing the numbers on.

At the `engine` boundary the shape is the one `PageHandle::set_content` and
`eval_to_string` already use, which distinguishes "no page" from "the page
refused":

```rust
pub fn screenshot(&self, options: ScreenshotOptions)
    -> EngineResult<Result<Vec<u8>, String>>
```

CDP maps the inner `Err` to `ProtocolError::server` (`-32000`) with the real
cause, at `Page.captureScreenshot`, `Page.printToPDF` and
`DOM.getLayoutMetrics`. The existing "empty `Vec` means failure" channel is
**not** reused: its message (`"Screenshot encoding failed"`) would lie about the
cause, against ADR-0015. The CLI's `render` and `dump` report the abort on
stderr and exit non-zero, for the same reason — a blank PNG with exit 0 is the
silent failure ADR-0015 removed everywhere else.

`Page::page_boundaries` runs its budgeted pass outside `reflow`, so it carries
its own landing pad; an uncaught abort there would kill the page thread, the
failure mode this whole boundary exists to remove. `Page::pdf` and
`Page::page_boundaries` each arm the budget **once, at the top**, so the flush
and the pagination pass share one deadline: two nested passes each arming their
own would let a single `Page.printToPDF` spend twice the limit, which is what
D1 refused when it said fifty iframes must not buy fifty deadlines.

### D8. Default 10 s, no knob yet

`DEFAULT_LAYOUT_BUDGET = Duration::from_secs(10)`, beside
`DEFAULT_SCRIPT_BUDGET`, configured by `PageOptions::layout_budget` beside
`script_budget`, with `Page::set_layout_budget` for a runtime change. No CLI
flag and no CDP parameter in v1: the script budget has neither either, and an
operator-facing knob belongs with the per-stage budgets that would give it
company rather than alone.

## Consequences

- A layout that outruns its budget, and a panic raised inside the layout pass,
  are now a typed error on one page instead of a wedged or dead page thread. The
  page stays alive and recovers on the next DOM or style change with no
  intervention.
- **Tests and benchmarks that build a `LayoutEngine` directly are unaffected.**
  They arm nothing, so the limit is `Duration::MAX` and `checkpoint()` is one
  thread-local read and a branch. Anything built on a `Page` is a different
  story and was not free: `PageOptions::default()` means `None` means 10 s, so
  the conformance runners inherited a live deadline the moment this landed. In
  a debug build layout is an order of magnitude slower, and a loaded CI machine
  tripping the deadline would emit a *blank* golden, reference or WPT report —
  a baffling pixel diff, not a timeout message. `cargo xtask
  golden|reftest|wpt|puppeteer|playwright` therefore set
  `layout_budget: Some(Duration::MAX)` explicitly: a committed fixture is not a
  hostile document, and those runners exist to be deterministic. `ContextOptions`
  and `NewPageOptions` gained the field alongside `script_budget` so the node
  runners could say so at all.
- **The stride's cost is below what the benchmark can resolve, and the
  measurement is reported as it came out.** `cargo bench -p oxidepage-layout
  --bench reflow`, each state given a discarded warm-up run first (the first
  run after a recompile measured ~50% slow regardless of which code was
  compiled — that confound is the reason for the warm-up):

  | case | before | after |
  | --- | --- | --- |
  | `full_reflow_1000_elements` | 5.752 ms | 5.714 ms (−0.7%) |
  | `incremental_relayout_1000_elements` | 114.5 µs | 117.0 µs (+2.2%) |
  | `styles_and_reflow_1000_elements` | 6.274 ms | 5.718 ms (−8.9%) |

  The signs point both ways, which is the tell: adding work cannot make
  `styles_and_reflow` 9% faster. Two runs of **identical** code on this machine
  differed by 30–45% while it was still busy and by ~10% once settled, so the
  spread swamps the effect. The most sensitive case is the incremental one
  (`compute_box_layout` runs once per box per pass) and it is the one that moved
  +2.2%; that is the honest upper bound available here, not a measured cost.
  (The absolute numbers are not comparable to ADR-0006 №15's 48.5 µs — different
  machine.)
- `LayoutEngine::reflow` gained a `Result`, which is the crate's **first**
  exported error type. About forty call sites in tests and benchmarks say
  `.expect("layout completes")`; the two production callers each make an
  explicit choice (D6, D7).
- **The `repeat(auto-fill, …)` case from ADR-0036 D5 is still not closed, and
  this ADR does not claim it.** Re-measured on the reproduction: `20000px` on
  both axes completes in ~1 s with **431 MB** of peak RSS. It never reaches the
  deadline, because it is not slow — it is *large*. A wall-clock budget bounds
  time, not memory, and bounding memory is a separate decision (a track/box-count
  cap, or an allocator limit) that this ADR deliberately does not take. What did
  change is the failure *mode* beyond 65 535 repetitions: taffy's `u16` overflow
  is now `LayoutAborted::EnginePanic` on a live page rather than a dead thread.
- **Residual holes, recorded rather than implied away:**
  - The inner loop of one taffy algorithm on one node (D2). No checkpoint can
    reach it without a taffy fork.
  - Peak memory, per the point above.
  - An abort raised by a synchronous geometry read never reaches the embedder
    on its own (D6). The turn measures zeros; the page recovers.
  - `style.resolve_styles` is inside the boundary but is not what the boundary
    is designed for: a panic in stylo's own traversal is now caught and reported
    as `EnginePanic` rather than killing the thread, which is an improvement, but
    no checkpoint is polled inside stylo, a *slow* restyle is unbounded, and the
    per-node style data such a panic leaves behind is not reclaimed by the
    recovery (D3).
- The engine now has three `catch_unwind` trust boundaries. The two older ones
  wrap a pure function of borrowed input; this one wraps a mutating pass and
  rests instead on discarding everything it touched (D3). A fourth should be
  argued for on its own terms again — the pattern here is "state an invariant
  that survives the unwind", not "wrap it and hope".
- WPT covers none of this. `crates/layout/tests/deadline.rs`,
  `crates/page/tests/layout_deadline.rs` and the unit tests in `budget.rs` are
  the whole regression surface, and every assertion in them is on layout output
  or a counter — never on wall-clock time, which flakes on a loaded machine
  (the rule `crates/layout/tests/grid.rs` states for the same reason).
