# ADR-0036: A UA limit on overlarge grids

- Status: accepted
- Date: 2026-08-14
- Builds on: ADR-0006 (layout via taffy + `stylo_taffy`), ADR-0008 D1 (the
  `catch_unwind` trust boundary around a hostile web font)
- Constrained by: design §2 P1 (assemble production components), §8 (security
  posture), §12 (deliberate v1 limits)

## Context

`crates/layout/src/construct.rs::taffy_style_for` turns stylo's
`ComputedValues` into a `taffy::Style` by calling `stylo_taffy::to_taffy_style`
and then patching the known conversion seams (`width: stretch`, physical
`justify-content: left|right`, the float fitter's ULP). Grid was the one seam
where the mismatch is not cosmetic.

Stylo represents grid line numbers and `repeat()` counts as `i32`. Taffy
represents them as `i16` and `u16`. The conversion narrows between the two
**unsafely**, and neither side bounds how many tracks a grid may generate.
Three defects follow, all reachable from a single CSS declaration:

1. **Process crash.** `stylo_taffy::convert::grid_line` narrows a named span's
   count with `line_num.try_into().unwrap()`, and `::track_repeat` does the same
   with a repetition count. `grid-template-columns: repeat(70000, 1px)` and
   `grid-column: span foo 70000` each panic with `TryFromIntError(PosOverflow)`.
   The unwind crosses `LayoutEngine::reflow`, which has no boundary, and takes
   the page thread — under the CLI, the process — with it.

2. **Memory blowup.** Taffy's occupancy matrix is dense: `CellOccupancyMatrix`
   allocates one byte per cell, `rows × cols`, sized up front from the items'
   placements and the explicit track count. One element with
   `grid-column/row: 1 / 32000` measured 1.01 GB of peak RSS and ~1 s;
   `grid-template-{columns,rows}: repeat(20000, 1px)` measured 411 MB. Both
   scale as N², and a document may hold many of them.

3. **Silent wraparound.** The remaining narrowing sites use `as i16` / `as u16`,
   so `grid-column: 1 / 100000` becomes line `-31072` — an unrelated line of the
   opposite sign — rather than a clamped one. This is also why the memory
   numbers look erratic across spans: some large values wrap to small ones and
   look harmless.

CSS Grid 2 [§Limiting Large Grids](https://drafts.csswg.org/css-grid-2/#overlarge-grids)
explicitly permits a UA to cap implicit track generation, so a limit here is a
conformant answer rather than a workaround. Every browser ships one.

The fix belongs in `layout`, not in a fork of `stylo_taffy`: the panic and the
narrowing are upstream bugs worth reporting, but the *limit* is a UA policy
decision that stays ours either way, and forking a pinned interop crate to
carry policy is the expensive shape (ADR-0006 pins stylo ↔ `stylo_taffy` ↔
taffy in lockstep).

## Decision

### D1. One classification decides everything; the pre-check is the primary guard

`taffy_style_for` calls `to_taffy_style_guarded`, which classifies the
element's grid values once (`classify_grid_values` → `GridFit`) and acts on the
answer:

- **`Absent`** — no grid property is set at all. Every narrowing site upstream
  lives inside a `grid_*` converter, so none is reachable: convert directly,
  with no landing pad and no second walk. This is the overwhelming majority of
  elements.
- **`Exact`** — every value narrows losslessly; convert upstream inside
  `catch_unwind`.
- **`Wraps`** — a line number exceeds taffy's `i16`/`u16` and upstream's `as`
  would wrap it; convert upstream, then re-derive the placement with
  `saturating_grid_line`.
- **`Refused`** — a value would panic the conversion; take the local mirror
  (D2).

The pre-check is primary and `catch_unwind` secondary on purpose: raising and
catching a panic for every element of a page full of hostile grids would fill
stderr with unwind messages for a case we can recognise by inspection. The belt
covers the seven `as` narrowings the classification does not model, which wrap
today but are one upstream edit away from unwrapping. No panic-hook suppression
is installed — `webfont::decode_caught` (ADR-0008 D1) does not either, and
swapping a global hook is racy in a process that runs many pages.

`AssertUnwindSafe` is sound at this call: the closure borrows
`&ComputedValues`, and `to_taffy_style` is a pure function of it, so an unwind
can leave nothing observable half-written.

### D2. The refusal path is a whole-style mirror, not a reset

`to_taffy_style_saturating` reproduces `to_taffy_style` field for field using
upstream's own **public** converters, changing only the two grid conversions
(`saturating_grid_line`, `saturating_grid_template_tracks`) and ending in
`..taffy::Style::DEFAULT` so a field added upstream takes its initial value
rather than failing to compile. `text_align` is the one deliberate omission:
upstream's converter is `pub(crate)`, and it only maps the three `-moz-*`
legacy keywords block layout uses for centering.

It has to be a whole-style mirror because `to_taffy_style` builds **one struct
literal**, so a panic anywhere in it loses the non-grid fields too. The obvious
cheap answer — return `Style::DEFAULT` with `display` kept — silently unstyles
the element, and measurably so: a
`position: absolute; left: 50px; top: 20px; width: 100px; height: 30px` grid
container laid out at `(0, 0) 300×0`, in flow, pushing its siblings. Worse, it
is *inconsistent*: `LayoutBox::position` is captured from stylo separately and
still says `Absolute`, so geometry and hit testing report the box as out of
flow while taffy laid it in flow. Mirroring keeps that class of disagreement
structurally impossible — only grid values differ from what upstream would have
produced.

The mirror also removes a cliff. `repeat(20000, 1px)` converts and clamps to
1000 tracks; without the mirror `repeat(70000, 1px)` produced *no* tracks,
purely because 70000 crosses a `u16`. The limit is one rule at every input
size.

### D3. The limit is 1000 tracks per axis, per side of the explicit grid

`MAX_GRID_TRACKS_PER_AXIS = 1000`, applied by `clamp_grid_tracks` to the
converted style:

- **Placements** (`grid_row`, `grid_column`): line indices clamp to
  `±(MAX + 1)` — grid lines are 1-based and may count back from the end of the
  explicit grid, and 0 is not a valid line — and span counts clamp to `MAX`.
  This is the half that bounds an item in an otherwise unremarkable grid.
- **Templates** (`grid_template_rows`, `grid_template_columns`): the budget is
  spent across the whole axis, not per `repeat()`. A per-repetition cap is
  evaded by `repeat(600, 1px) repeat(600, 1px)`, or by a plain list of 100 000
  single tracks — both linear in stylesheet bytes but quadratic in matrix
  cells. The walk charges 1 per `Single` and `count × tracks.len()` per
  `repeat()`, reduces the repetition that crosses the budget, and truncates the
  rest of the list. Line names are truncated with it: upstream emits one set per
  line and taffy pairs set *i* with component *i*, so dropping tracks alone
  would shift that pairing.
- **Named areas** — see D4.

1000 is chosen to be far above any real layout — the largest grids in the wild
are in the low hundreds of tracks — and low enough that the worst case is
2000×2000 cells (both sides of both axes), about 4 MB. The clamp is a **ceiling
only**: a grid at or under it converts exactly as it did before, which is what
the control test in `crates/layout/tests/grid.rs` pins.

### D4. `grid-template-areas` is clamped too, because the amplification is cross-axis

Taffy sizes the explicit grid per axis as `max(template tracks, area extent)`,
so an area count on **one** axis multiplies against the clamped track count on
the **other**. Within a single axis the names really are their own bound — an
N×M named grid needs N × M names in the stylesheet — but the second axis is
supplied for free by one item's capped placement: 30 000 named columns (60 KB
of CSS) against a `grid-row: 1 / 1001` item measured 57 MB of peak RSS versus a
24 MB baseline, a ~500× byte-to-byte amplification. `clamp_grid_areas` drops
areas that start past the capped grid and clamps the rest, which brings that
page to 27 MB.

Named areas contribute no *tracks* to an axis in taffy's sizing pass, only
occupancy, so this clamp is invisible to layout geometry — it is covered by a
unit test in `construct.rs` rather than by a rect assertion.

### D5. `repeat(auto-fill, …)` is **not** bounded by this change

An auto-repetition's count is resolved by taffy at layout time from the
container's used size, not from the stylesheet, so no style-level cap can reach
it. Both auto counts pass through the clamp untouched, each charged one
repetition's worth of tracks against the axis budget.

The exposure is real and pre-existing, and it is recorded here rather than
papered over:

- `width: 20000px; height: 20000px` with `repeat(auto-fill, 1px)` on both axes
  measured **411 MB** of peak RSS from ~150 bytes of CSS, before and after this
  change alike.
- Past 65 535 repetitions taffy's own arithmetic breaks: `explicit_grid.rs:172`
  computes `(floor(repetitions) as u16) + 1` — `attempt to add with overflow`
  in a debug build, a silent wrap to **zero** tracks in release — and
  `types/coordinates.rs:133` overflows the matching subtraction.
  `width: 200000px` with `repeat(auto-fill, 1px)` reproduces the first;
  `60000px` on both axes reproduces the second. Both panics are raised inside
  taffy's layout pass, so `to_taffy_style_guarded`'s `catch_unwind` — which
  wraps the *conversion* — does not see them.

Bounding this needs a boundary around the layout pass itself, which is
PLAN.md §1.2 (a layout deadline with its own `catch_unwind`), or an upstream
taffy fix. Both are out of scope here, and this ADR claims neither.

## Consequences

- Authored grid track counts and placements are bounded and non-fatal. The
  reproductions in `crates/layout/tests/grid.rs` all lay out and return
  geometry; before this change two of them aborted the page thread and the rest
  allocated by the hundred megabytes (`grid-column: 1 / 32000` on both axes:
  1008 MB → 26 MB; `repeat(20000, 1px)`: 411 MB → 26 MB).
- **`repeat(auto-fill, …)` is not covered** (D5). "Grid is bounded" is true of
  authored counts only.
- Grids within the limit are untouched. `taffy_style_for` gained one
  classification walk per element and, for elements that declare grid values, a
  `catch_unwind` landing pad; the reflow benchmark moves by ≈1%, the same
  magnitude as the spread between two runs of identical code (a measurement of
  an earlier revision of this change came out 4% *faster* on the same machine).
  Elements with no grid property take neither the walk's second half nor the
  landing pad.
- The engine now has two `catch_unwind` trust boundaries: hostile font bytes
  (ADR-0008 D1) and hostile grid values. Both wrap a pure function of borrowed
  input, both map a caught unwind to a documented degradation, and neither
  suppresses the panic message. A third one should be argued for on the same
  terms, not added by analogy.
- `to_taffy_style_saturating` mirrors an upstream function and will drift from
  it if the pinned `stylo_taffy` revision changes `to_taffy_style`'s field set.
  `..Style::DEFAULT` bounds the damage to "a new field takes its initial value
  on the refusal path", and the refusal path needs a `repeat()` or span count
  above 65 535 to be reached at all. Upstream clamping instead of unwrapping
  would delete the mirror, `saturating_grid_line` and the classification; the
  per-axis limit in D3/D4 would stay, because it is policy.
- WPT does not cover any of this: no `css/css-grid` subset is vendored under
  `tests/wpt/vendor`, so `crates/layout/tests/grid.rs` and the unit tests in
  `construct.rs` are the whole regression surface for the limit.
