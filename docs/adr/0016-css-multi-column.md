# ADR-0016: CSS multi-column layout as clipped views of one continuous flow

- Status: accepted
- Date: 2026-07-12

## Context

stylo parses and cascades `column-count` / `column-width` (the `layout.columns.enabled`
pref is on), but nothing in `crates/layout` read them: a multicol container laid out as a
single tall column. Real content hits this — vuejs.org's footer is `column-count: 3` — and
it was the last visible difference from Chrome after ADR-0015.

The textbook implementation is **block fragmentation**: split the content into fragments
and stack them side by side. Two things in this codebase make that a rewrite rather than a
feature:

- **taffy does not fragment.** There is no fragmentainer concept to hook into; every
  compute function returns one box with one size.
- **The box tree cannot represent one DOM node at two positions.** `node_to_box` is a 1:1
  `HashMap<NodeId, BoxId>` (`layout/src/tree.rs`) and the painter's `origins: Vec<Point>`
  is indexed by `BoxId` (`paint/src/builder.rs`) — one origin per box, by construction.
  Splitting a box into continuations means reworking paint, geometry, hit-testing and the
  IFC model at once.

`column-rule-*` and `column-fill` do not exist in this stylo build at all: they are
`engine = "gecko"` in `stylo-0.19.0/properties/longhands.toml`, so the `column` style
struct here carries only `column_count`, `column_width` and `column_span`. They cannot be
read from the cascade, whatever we implement.

## Decision

**Lay the content out once, and show each column as a clipped, translated view of that one
continuous flow.**

A `BoxKind::MulticolRoot` owns exactly one child: an anonymous **flow** box holding all of
the element's content, built by the ordinary `collect_flow_children` (so `::before`/`::after`,
anonymous wrapping of inline runs, and promotion to an IFC root all still happen). The
compute arm lays that flow out once at the used column width with an unbounded block size,
then slices its block axis:

1. `used_columns()` — the CSS Multicol §3.4 pseudo-algorithm gives the used count and width.
2. `break_opportunities()` — the class-A break points of CSS Fragmentation §3: the top
   border edge of each in-flow block child, and each parley line top. Monolithic content
   (replaced boxes, tables, flex/grid, scroll containers, a nested multicol) contributes
   only its own top edge. **Because a boundary is only ever taken at one of these, no line
   is ever cut in half.**
3. `balance()` — a greedy fill under a binary search for the smallest column height that
   fits in N columns, then snapped to an achievable height and re-filled.

Paint emits, per column, `PushClip(column rect)` → `PushLayer { opacity: 1.0, transform:
translate(x, -start) }` → the whole flow → `PopLayer` → `PopClip`. Both backends already
honour that ordering: a clip path is mapped through the *current* transform (raster
`canvas.rs`, PDF `content.rs`), so the clip must be pushed **outside** the layer or it
would be translated along with the content and clip nothing. `PushLayer` at `opacity: 1.0`
costs a CTM push and no pixmap.

Correct pixels then fall out for free. Text flows across the column boundary because it is
one parley layout. A block straddling a boundary has its background sliced by the clip —
which is exactly `box-decoration-break: slice`, the CSS default. `paint_ifc` needs no
column awareness: lines outside a column's slice are simply clipped away.

`multicol::map_flow_point` is the single definition of the column transform; paint,
`geometry::absolute_origin`, `client_rects` and `hit_box` all agree with it by construction.

### Two coordinate spaces, one structural walk

Boundaries are chosen twice from the same walk. During compute they come from
`unrounded_layout` (to pick *which* opportunities the columns end at, and hence the
container's height); after `taffy::round_layout` they are re-derived from `final_layout`,
because paint positions the flow's content from the **rounded** origins. A boundary taken
from the unrounded ones would sit up to half a pixel off and shave the top line of every
column but the first. `break_opportunities` therefore returns a list whose length and order
are identical in both spaces, and `MulticolContext` stores *indices* into it rather than
pixels.

### The flow box is a containing block for all out-of-flow descendants

`positioning::establishes_containing_block` returns true for a multicol flow box,
`position: fixed` included. Paint reaches a hoisted out-of-flow box through the static
parent it was built under — which is *inside* the flow, i.e. inside the column's clip and
transform. A box whose position had been resolved against a containing block *outside* the
flow would be painted from in there anyway, and the column transform would be applied to a
coordinate that never had it: a double transform. Keeping every box under a multicol root
in one coordinate space is what lets paint, geometry and hit-testing share one mapping rule.

### Paint-phase barriers

A multicol root stops `compute_positioned_inline` from propagating its flow's flag upward,
and emits the positioned-inline pass itself, once per column, inside the clip and layer.
Otherwise the nearest positioned *ancestor* would walk down into the flow and paint that
text again, unclipped and untranslated, straight down the page.

Relatedly, `paint::text` now resolves an IFC's terminating element with `ifc_element()`
(the nearest boxed ancestor) instead of `box.dom_node`. On an anonymous box that is `None`,
so the positioned-inline ancestor walk never terminated and reported `true` for *any*
`<span>` under *any* positioned ancestor. A multicol flow box is anonymous, but so is every
inline-run wrapper in a mixed container — this was a latent bug, fixed here because multicol
makes it unavoidable.

## Consequences

Multicol works for the case that matters: content flows across columns, lines stay whole,
straddling backgrounds slice, and the vuejs.org footer renders in three columns. It cost
one new `BoxKind`, one side context, one compute arm, and no change to either rasterizer.

**The flow subtree is emitted once per column.** That is the price of not having a fragment
tree: the display list carries `N × |flow items|`, each copy clipped to its column.
`MAX_COLUMNS = 64` bounds it. A per-column bounding-box cull is the obvious follow-up.

Not supported in v1, and why:

| Not supported | Why |
|---|---|
| `column-rule-*`, `column-fill` | `engine = "gecko"` in this stylo build — not in the cascade at all. Columns therefore always balance when the block size is `auto`, and always fill when it is definite. |
| `column-span: all` | Cascades, but a spanner splits a multicol into stacked segments — a second, orthogonal box-tree shape. A `column-span` element simply flows inside a column. |
| `break-before` / `break-after` / `break-inside`, `orphans` / `widows` | The break list has no forced breaks and no penalty model. Forced breaks are a small extension (a flagged opportunity the fill must take); orphans/widows need a constrained fill. |
| Multiple client rects for a **block** straddling a break | The box tree is 1:1 with the DOM, so a block has exactly one border box: `getClientRects()` reports one rect, in the column holding its *top*. An **inline** element does get one rect per column. Painting is correct either way; only the geometry API is coarse. Precedent: geometry already ignores transforms (ADR-0013). |
| Spec-exact out-of-flow containing block | An out-of-flow descendant is contained by the *flow*, not the container, so `bottom: 0` lands at the bottom of the last column and `position: fixed` inside a multicol is not viewport-pinned. Deliberate — see above. |
| Exact intrinsic sizes (§9) | `min-content` ≈ one column, `max-content` ≈ N columns side by side. Only observable for a floated / inline-block / flex-item multicol container. |

WPT `css/css-multicol` is not vendored in this change: `fetch-wpt` needs network and
rewrites the whole vendor tree. Coverage is unit tests (`layout/tests/reflow.rs`,
`paint/tests/multicol.rs`), a display-list golden, two pixel reftests, and page-level
`getBoundingClientRect` / `elementFromPoint` tests. The existing WPT run is unchanged.
