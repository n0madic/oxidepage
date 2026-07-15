# ADR-0006: Phase 5 layout-engine (taffy + parley) implementation decisions

- Status: accepted
- Date: 2026-07-05

## Context

Phase 5 (design doc §10) gives the engine real layout: a box tree, taffy for
block/flex/grid, parley for inline formatting contexts (text shaping and line
breaking), scrollable overflow, and the full JS geometry surface
(`getBoundingClientRect`, `getClientRects`, `offset*`/`client*`/`scroll*`,
`elementFromPoint`, resolved values in `getComputedStyle`, scroll offsets).
The reference implementation is **blitz-dom 0.3.0-alpha.6** (MIT OR
Apache-2.0): its layout modules are the only production integration of
stylo + taffy + parley, and substantial portions of `crates/layout` are
adapted from it (with attribution in the module docs). This ADR records
where OxidePage deliberately deviates and the v1 limitations.

## Decision

1. **Stack: `stylo_taffy` (Blitz git) + `taffy =0.12.1` + `parley 0.10`,
   `rust-version` 1.85 → 1.89.** `stylo_taffy` is the only stylo↔taffy interop
   crate, it must match our stylo pin, and it pins taffy exactly; all three
   (plus stylo) must be upgraded **in lockstep**. Both Blitz crates require
   Rust 1.89; CI runs stable everywhere, so the `rust-version` bump is safe.
   Taffy's feature set mirrors blitz-dom plus `float_layout`.

   Originally `stylo_taffy =0.3.0-alpha.6` + `taffy
   =0.11.0-experimental-cache-fix.3`. When stylo moved to `0.19.0` (ADR-0005),
   no published `stylo_taffy` release targeted it, so the dependency is pinned
   to the Blitz revision that does, which in turn pins taffy `=0.12.1`.

2. **A separate box tree in `crates/layout`, not Blitz's boxes-on-DOM-nodes.**
   Blitz stores taffy styles, caches, and layouts directly on DOM nodes and
   allocates anonymous boxes in the DOM slab. OxidePage keeps the DOM free of
   taffy/parley state: `LayoutTree` is a flat arena of `LayoutBox`es,
   `NodeId ↔ BoxId` is a side map, and anonymous boxes exist only in the box
   tree. Reasons: the `dom` crate stays layout-free; anonymous boxes would
   pollute the arena and invalidate the version-guarded CSSOM caches; and a
   full tree rebuild is a plain `Vec` drop. Everything the compute phase
   needs (taffy style via `stylo_taffy::to_taffy_style`, parley text styles,
   `text-align`/`text-indent`, replaced sizes, `position`/`z-index`) is
   **captured at construction time**, so the taffy/parley passes never touch
   the DOM or stylo — `enter_active_tree` is only held during construction.

3. **Construction is two-phase so parley contexts never nest.** Phase 1
   builds the structural box tree and records each IFC root with its
   participating DOM nodes; phase 2 shapes each IFC sequentially. An
   inline-block inside an IFC starts its own IFC, which would otherwise
   re-borrow the shared `LayoutContext`/`FontContext` (this mirrors
   blitz-dom's deferred-construction phase).

4. **Floats are not laid out** (`floats` feature off, per design §12). Floated
   elements keep their boxes but behave as in-flow content.

   > **No longer true.** The `floats` (stylo_taffy) / `float_layout` (taffy)
   > features are now on: a float is taken out of flow, block boxes overlap it,
   > and `clear` resolves against it. What is still missing is float-aware
   > *inline* layout — a line box beside a float keeps its full width instead of
   > shortening, so text paints over the float. See design §12.

5. **No block-in-inline splitting.** An inline element with an in-flow block
   descendant is treated as a block-level child (no anonymous splitting of
   the inline). This matches blitz-dom's model.

6. **Absolute positioning is against the direct parent box** (taffy's model),
   not the nearest positioned ancestor through static intermediates —
   a deviation from design §5.7; re-parenting abs-pos boxes in the box tree
   is post-v1. `fixed` behaves as absolute from its parent chain; `sticky`
   behaves as relative.

7. **Geometry ignores transforms and writing modes** (bidi within lines works
   through parley). Hit-testing uses approximate paint order (positioned by
   `z-index` above in-flow above negative-`z` positioned), ignores
   `pointer-events`, and does not descend into iframes (none exist yet).
   Inline hits attribute text runs to their owning span via the parley brush
   (a packed `NodeId`).

8. **Replaced elements:** `<img>`/`<canvas>`/`<svg>` are sized from
   `width`/`height` attributes and CSS only — intrinsic sizes arrive with
   image decoding in Phase 6; the aspect ratio falls back from style to the
   intrinsic ratio to the attribute ratio, and a missing ratio never
   fabricates NaN dimensions. Text controls use blitz-dom's simplified leaf
   sizing (300px default width, `rows`/`cols` — clamped to ≥ 1 — for
   `<textarea>`).

9. **Scroll: real, clamped offsets; `scroll` events as tasks.** Offsets live
   on the `LayoutEngine` (they survive rebuilds), are clamped on write and
   re-clamped on read against the current overflow, and script writes queue
   scroll targets that the page's event loop dispatches as non-bubbling
   `scroll` events (document scroll fires on the document). No
   `scrollIntoView`, no smooth scrolling, zero-width scrollbars; rAF and
   "update the rendering" arrive in Phase 6.

10. **Resolved values** in `getComputedStyle` are used values for
    `width`/`height`/`top`/`right`/`bottom`/`left`/`margin-*`/`padding-*`
    when the element generates a box (computed values otherwise, e.g.
    `display: none`). Inset used values are measured against the parent box
    (see decision 6) and only for absolutely-positioned boxes; static,
    `relative`, and `sticky` boxes report the computed value (for `relative`
    the computed offset *is* the used value).

11. **WPT gates: `css/cssom-view` + `css/css-flexbox` testharness subsets**
    replace the design §10 exit criterion of flexbox/grid *reftests*, which
    cannot run before paint exists (Phase 6). The flexbox and cssom-view
    vendoring filters commit only files containing `testharness.js`
    (`css/css-flexbox` upstream is ~90% reftests); iframe-dependent,
    `scrollIntoView`, `visualViewport`, smooth-scroll, and `matchMedia`
    listener tests are skipped with comments in `xtask/src/wpt.rs`.
    `css/css-grid` is not vendored in Phase 5 (its tests sit in
    subdirectories the flat vendoring doesn't cover; grid is covered by unit
    and page tests) — it comes with Phase 6.

12. **Tables are laid out as CSS grid** (port of blitz-dom `table.rs`):
    cells become grid items with row/column placement (col/rowspan via
    dense auto-flow); `<tr>`/`<tbody>`/captions/colgroups generate no boxes
    in v1, so their geometry APIs report nothing. A cell border with style
    `none` contributes zero width to the collapsed-border gap (stylo keeps
    the specified width in the computed border struct — blitz-dom inherits
    that bug).

13. **Stylo layout prefs are set in `DomTree::new`** (in addition to
    `StyleEngine::new`): inline `style=""` attributes are parsed during
    document parsing, which can run before any `StyleEngine` exists —
    without the prefs, pref-gated properties (grid, columns) silently drop
    from inline declarations.

14. **Fonts: Ahem is bundled and always registered**; without the
    `system_fonts` feature it also backs every generic family. All
    metric-dependent tests specify `font-family: Ahem` (1em × 1em glyphs,
    0.8em ascent), so layout tests are byte-identical across platforms. The
    style engine's font-metrics provider (`ex`/`ch`/`ic` units) is a
    factory-installed parley/skrifa provider sharing the layout engine's
    font collection.

15. **Benchmark numbers (2026-07-05, Apple-silicon macOS, release):** the
    read-modify-write budget (≤ 10 ms/iteration on a ~1000-element page with
    a full rebuild) holds with ~5× headroom even before incremental
    relayout; the incremental patch (decision 16) then gains another ~30-55×.

    | benchmark | full rebuild | incremental (WP-K) |
    |---|---|---|
    | `full_reflow_1000_elements` (construct + compute, warm styles) | 2.69 ms | — (forced rebuild) |
    | `incremental_relayout_1000_elements` (one leaf width change) | — | 48.5 µs (≈55×) |
    | `styles_and_reflow_1000_elements` (cold cascade + reflow) | 3.18 ms | — |
    | `geometry_read_modify_write` (JS style write + `offsetWidth` read) | 2.10 ms | 65.9 µs (≈32×) |

16. **Incremental relayout is snapshot/pointer-diff based, not blitz's
    RestyleDamage bits.** After a restyle with an unchanged DOM structure
    (`DomTree::structure_version`: child-list, character-data, and
    non-`style`/`class`/`id` attribute mutations bump it), the engine diffs
    every styled element's computed-style `Arc` pointer against a per-build
    snapshot. Non-structural changes on box-generating elements are patched
    in place (fresh taffy style + captured fields; taffy caches cleared
    along the ancestor chain only); anything else — display/position/float
    changes, IFC-contributor or pseudo-style changes, text-affecting changes
    on IFC roots, boxless elements, table parts — falls back to a full
    rebuild. Text-relevant structs are compared by pointer first (inherited
    structs stay shared) with a value-equality fallback (re-specified but
    identical declarations get fresh structs). This trades blitz's
    fine-grained damage bits (which need spare `RestyleDamage` bits threaded
    through stylo) for an O(elements) pointer walk that is trivially
    correct-by-construction; the benchmarks gain ~55× (pure relayout) and
    ~32× (through JS) — see decision 15. Paint-only changes (identical taffy
    style translation) update captured styles without clearing any taffy
    caches. `NodeFlags::LAYOUT_DIRTY` remains unused.

## Consequences

- Layout correctness is fully testable headlessly (geometry assertions with
  Ahem), but visual correctness (paint, reftests) waits for Phase 6.
- The experimental taffy pin means taffy/stylo_taffy/parley/stylo upgrades
  are a single coordinated change validated by the layout test suite.
