# ADR-0026: Transform-aware geometry, actionability primitives, and paginated capture

- Status: accepted
- Date: 2026-07-27

## Context

ADR-0013 landed CSS transforms at **paint** time and said so in its own limits list:

> Geometry (`getBoundingClientRect`, hit-testing, scrollable overflow) still ignores
> transforms; only paint applies them.

`transform` became a `Transform2D` in `paint::convert::transform` and rode on
`DisplayItem::PushLayer`; the rasterizer kept a CTM stack. So a `translateY(-100%)`
off-canvas panel *rendered* where it belongs while `getBoundingClientRect()` reported the
untransformed box and `hit_box` stayed a plain axis-aligned containment test.

Both drivers this engine is heading for (`docs/automation-roadmap.md`, stages 6–10) compute
a click point from element geometry. So **a click on any transformed element landed
somewhere else** — carousels, off-canvas menus, every `scale()` affordance — no matter which
protocol sits on top. That is not a rendering nicety; it is the difference between an
automatable page and an undriveable one.

Two capture gaps sat next to it. `export-pdf` wrote **one page as tall as the entire
document**, which is not what anyone means by "print this page". And a screenshot could only
be the viewport or the whole page: no clip rectangle, no JPEG.

## Decision

### D1. One transform resolver, moved down into `layout`

`paint::convert::transform` was the only place a CSS transform became a matrix, and it was
`pub(crate)` in `paint` — which `layout` cannot reach, because `paint` depends on `layout`
and not the reverse. Writing a second resolver in `layout` is how paint and geometry drift
apart, so the function **moved** to `crates/layout/src/transform.rs` and `paint::builder`
now calls it. This is `multicol::map_flow_point`'s rule applied again: one definition of a
mapping, every consumer agreeing by construction.

The returned matrix lives **in the space of the `border_box` passed in**, transform origin
baked. That single property is what lets one function serve every caller: paint passes the
absolute border box and gets an absolute matrix; layout resolves against the *local* box
(`0, 0, w, h`) and gets one that hit-testing can invert directly. The two differ by exactly
a conjugation, which `Transform2D::at_origin` performs.

The individual properties `translate` / `rotate` / `scale` — ADR-0013's *second* named limit
— landed with the move, composed per CSS Transforms 2 in the order translate → rotate →
scale → `transform`. Because they share the resolver, paint and geometry gained them
together and cannot disagree about them.

### D2. `layout` caches one matrix per transformed box; geometry never re-derives it

`LayoutEngine::border_box(node)` takes no `&DomTree`, and neither do the dozen call sites
that reach it (`Element.getBoundingClientRect`, `input_synth::offset_in`,
IntersectionObserver, lazy-image visibility). Geometry therefore has no access to computed
styles and cannot call the resolver itself.

So a post-layout pass (`transform::resolve_transforms`, run from `reflow` after
`round_layout`) resolves every transformed box once, against its local border box, and
caches the result on `LayoutBox::transform`. It runs after rounding because
`transform-origin: 50%` and `translate: 50%` resolve against the **used** border-box size,
and because geometry and hit-testing read the rounded boxes.

A *flip* of that flag on the incremental patch path forces a full rebuild:
gaining or losing a transform changes which boxes this one is the containing
block for, and hoisting runs only on the rebuild path — patching in place would
leave an out-of-flow descendant resolved against the old containing block while
paint and geometry compose the new ancestor matrix over it. Animating an
existing transform's *value* does not flip it and stays on the fast path.

A companion `LayoutBox::has_transform` bool is captured at construction — the same slot
pattern `pointer_events_none` and `z_index` already use. Two passes need the answer before
the matrix exists: `positioning` runs before layout, and the resolve pass uses it to skip
the overwhelming majority of boxes. It subsumes the old `positioning::has_transform`, which
re-read styles per box and only knew about the `transform` shorthand; a `rotate:`-only
element now correctly establishes a containing block for its absolute *and* fixed
descendants.

### D3. Geometry composes ancestors, and pays nothing when nothing is transformed

`absolute_frame(box_id, include_scroll) -> (Point, Option<Transform2D>)` is the new choke
point; `absolute_origin` is unchanged and remains its first field. It walks the ancestor
chain and, when no box on it carries a transform — the overwhelming case — returns exactly
today's answer with `None`. Otherwise it composes the transformed ancestors innermost-first
(`acc.then(&outer)`), which is the order the rasterizer's CTM stack applies them in.

The composed matrix lives in the *same* space as the returned origin: element scroll is
subtracted along the way and document scroll at the end, and conjugating each box's local
matrix by its own origin **in that space** distributes over the composition. A viewport-space
matrix is exactly a document-space one conjugated by the scroll, so the two spaces cannot
disagree.

Transform-aware as a result: `border_box`, `padding_box`, `client_rects`,
`bounding_client_rect`, and therefore `getBoundingClientRect` / `getClientRects`,
`Page::layout_rect`, IntersectionObserver rects, lazy-image visibility, and
`MouseEvent.offsetX/offsetY` (which is measured from `padding_box`).

### D4. Hit-testing inverts per box, where multicol already does

`hit_box` receives `pt` relative to the box's border-box origin — precisely the space the
cached local matrix lives in. The inverse goes in **at the top**, before the containment
test: the probe comes back through `matrix.inverse()` before anything is compared against
the box's own untransformed geometry, and a singular matrix (`scale(0)`) means no hit, as in
every browser.

That is the same position and shape as the existing multicol arm
(`multicol::unmap_content_point`) — this codebase's own precedent for inverting a paint-time
mapping during hit testing. Input synthesis needed no change at all: `input_synth::hit_test`
already routes through `elements_from_point`, and `offsetX/offsetY` already derives from
`padding_box`. `clientX/clientY` stay the raw probe coordinates.

`offsetX`/`offsetY` do **not** measure the probe against the transformed
bounding box — that reports 100 for a click at the visual centre of a
`scale(2)`-ed 100 px box, and on a rotated one measures from a corner that is
not a corner of the element. `LayoutEngine::offset_in_element` sends the probe
back through the inverse of the element's frame instead, so the offsets are the
element's own coordinates whatever the transform.

### D5. Actionability primitives, with one implementation each

```rust
Page::content_quads(node) -> Vec<[Point; 4]>
Page::scroll_into_view_if_needed(node, rect: Option<Rect>) -> bool
```

`content_quads` is `client_rects` mapped with `map_quad` instead of `map_rect` — the
un-bounding-boxed form of what D3 already computes, so a rotated element reports the
quadrilateral a click has to aim inside of rather than a rect that is mostly empty space.
Both come from one private `client_frames`, so they cannot report different geometry.

A scroll offset lives in the scroller's **own** content px while those rects are
visual, so `scroll_into_view` maps them back through
`unmap_into_scroll_space` before doing arithmetic: under a `scale(2)` ancestor
the visual delta is twice the scroll the container needs, and scrolling by it
overshoots (and stops being idempotent).

`scroll_into_view_if_needed` is "align `Nearest` on both axes", which `Element.scrollIntoView`
already implemented — in **bindings**, where `Page` cannot reach it. The whole algorithm
moved down to `crates/layout/src/scroll_into_view.rs`, returning the list of scroll targets
that changed; `imp::element::scroll_into_view` and the new `Page` method both call it and
both queue `scroll` events from that list, *after* the layout borrow is released. The
multicol lesson again: one definition, two callers. Layout never re-enters JS.

### D6. A screenshot clip *is* the existing (size, scroll) pair

`raster_skia::render_sized(list, options, size, scroll)` already took both, and a clip
rectangle in document coordinates is exactly `size = clip.size, scroll = clip.origin`. So
`render_clipped` is a four-line entry point and `RasterOptions` is **untouched**, which keeps
`xtask/src/reftest.rs` and the whole reftest corpus out of the blast radius. `position: fixed`
content pins to the clip's own top-left, as it would in a viewport at that scroll position.

JPEG cost a dependency edge rather than a build: the workspace already carries `image` with
the `jpeg` feature, and `paint` already pulls it in. The format choice belongs at the page
level, so `ScreenshotOptions` / `Page::screenshot_with` live in `crates/page/src/render.rs`;
`screenshot(dpr)` and `screenshot_full_page(dpr)` stay as thin wrappers and no existing
caller changed. A `clip` wins over `full_page`: both name a capture area in the same
coordinates, and the explicit one should.

### D7. PDF pagination: one content stream, N pages referencing it

Four sub-problems, four specific answers.

**(a) Where the breaks come from: layout, not the display list.** `paint::text::paint_ifc`
emits `GlyphRun`s whose `y` is a *baseline*; no `DisplayItem` carries a line top or bottom.
So `crates/layout/src/pagination.rs` asks `multicol::break_opportunities` — the class-A break
points ADR-0016 credits with "no line is ever cut in half" — with the flow rooted at the
document box, and fills pages greedily against them. `export-pdf` stays a dumb display-list
consumer (design P5) and is handed finished boundaries.

**(b) How pages are emitted.** Slicing `list.items` mid-stream would mean re-opening every
`PushClip`/`PushLayer` still open at the cut, and the display list offers no way to ask what
is open at item *i*. So **nothing is sliced**: the document's content is emitted **once** as a
Form XObject in document coordinates, and each page is a ~100-byte content stream that clips
to the page's content box, translates by its slice offset, and invokes that one form. Form
XObjects were already in use for transparency groups, resources are already global, and font
subsets already accumulate across the whole stream. The rejected alternative — repeating the
full stream per page — is correct but O(pages × content).

**(c) `printBackground` is a *build* option, not a PDF one.** By export time an element
background is an ordinary `Fill`, indistinguishable from any other. So it is a
`PaintOptions { print_background }` threaded into `paint::builder`, suppressing
`background::paint` and the propagated canvas colour while keeping the opaque white base and
replaced content. `Page` already rebuilt an uncached list for PDF, so the second build costs
nothing new. `Page::pdf` therefore takes **two** option structs, one per layer, which is the
honest shape.

The per-page clip is the page's **slice**, never its full content height. The
fill normally stops at a break opportunity above the page bottom, and the strip
below it holds the *next* page's content — clipping to the full box drew it at
the foot of one page and again at the head of the next, which is the cut-line
artefact pagination exists to prevent. Identical rule, identical reason, to the
per-column clip in `paint::builder`. For the same "must agree" reason the
document box is the **content** width (not the viewport's): fit-to-width measures
against it, and a narrower box shrank the page to fit content the form XObject's
`/BBox` then clipped away.

**(d) Wide content is fit-to-width scaled.** There is no print-media relayout, so a 1280 px
document on A4 (~717 px of content width) would run off the edge. The export scales by
`min(1, content_width / document_width)` and then by the user's `scale`. It never magnifies.

### D8. One display-list builder

`paint` exported three entry points by the end of this work: `build_display_list`,
`build_display_list_with` (the new `PaintOptions` form), and
`build_display_list_full`. The last was an alias whose body had been identical to
the first since the list became scroll-independent (ADR-0007 D8) — a name
promising a "full page vs viewport" distinction that the builder does not make.
Three entry points for one operation is three places to drift.

There is now **one**: `build_display_list(dom, engine, &PaintOptions)`. A viewport
render and a full-page one differ only in what the *rasterizer* is told to cover
(`render_scrolled` vs `render_full_page` vs `render_clipped`), which is where the
difference actually lives. `Page` keeps a single private
`full_page_display_list(&PaintOptions)` helper, uncached — the paint stamp knows
nothing about `PaintOptions` and cannot tell a `print_background: false` build
from an ordinary one. (ADR-0007's prose still names `build_display_list_full`;
that record stands as written, and this is the entry that supersedes it.)

### D9. Deliberate divergences from the plan and from Chrome

- **`printBackground` defaults to `true`**, where Chrome defaults it to `false`. `render -o
  page.pdf` should keep meaning "the page as it looks"; a driver that wants Chrome's default
  says so.
- **`PdfOptions::default()` paginates.** `paginate: false` restores the old single tall page
  byte-for-byte, and the two export-pdf tests that asserted the old geometry now say so
  explicitly.
- **Pagination's greedy fill differs from multicol's in one rule.** A column with no break
  opportunity inside it simply overflows (`multicol::fill`); a *page* cannot, because an
  overflowing page is a page of lost paper. A `<body>` that is a flex container, or a single
  tall block, offers no class-A break point **at all** and would print as one page as tall as
  the document — the very bug pagination exists to fix. So a page holding no opportunity
  breaks at the page boundary instead, which is CSS Fragmentation §3.4's last-resort rule.
- **`Page::pdf(&PdfOptions, &PaintOptions)`**, not the single-struct signature the plan
  sketched, for the layering reason in D7(c).

## Consequences

A click computed from element geometry lands on the element, transformed or not, and that
is asserted end-to-end: `crates/page/tests/input.rs` clicks a `translate(200px, 300px)`
button at its painted centre and watches it activate, and at its untransformed position and
watches nothing happen. Everything an actionability check needs — quads, "is it visible",
"scroll it into view" — is now a `Page` method with one implementation behind it.

One WPT expectation flips to PASS with no suppression:
`css/cssom-view/GetBoundingRect.html :: getBoundingClientRect`, which is the transform case
directly. The plan predicted the `elementFromPoint` / `elementsFromPoint` transform subtests
would flip too; they do not, and the reason is worth recording rather than retrying:

- `elementFromPoint.html :: transformed element at x,y` and
  `elementsFromPoint.html :: transformed element at x,y` also require hit-testing *inside* an
  inline `<svg>` (`svg.querySelector("rect")`). Inline SVG is rasterized as an image
  (ADR-0013), so those `<rect>`s generate no boxes and nothing can hit them. Orthogonal to
  transforms.
- `elementsFromPoint-simple.html :: … 3d transform` asserts the full stacking chain under a
  genuine 3D transform — both a stacking-context tree and real 3D, and both non-goals below.

`HTMLImageElement-x-and-y-ignore-transforms.html` **did not move**, which is the point: it is
the guard on the carve-out in the limits table.

### Deliberate limits

| Not supported | Why |
|---|---|
| **`offset*`, `client*`, `scrollWidth`/`scrollHeight` stay untransformed** | CSSOM-View defines them on the *untransformed* border/padding box. This is a correction to the roadmap text, which listed `offset*`/`client*` as transform-aware: making them so would **regress** `css/cssom-view/HTMLImageElement-x-and-y-ignore-transforms.html`, which passes today precisely because they ignore transforms. It narrows, rather than closes, ADR-0013's first limit — transformed scrollable overflow lives in blitz-derived `layout/src/overflow.rs`, whose own header says "without transforms". |
| **`ResizeObserver` reports the untransformed border box** | CSS Resize Observer observes the *observed box*, not the visual one: `scale(2)` must not double `borderBoxSize`, and changing a transform must not fire a notification at all. `border_box` becoming transform-aware would have done both, so the observer reads a new `LayoutEngine::border_box_size` instead. Same family as the `offset*` carve-out above. |
| **3D transforms stay flattened** to the 2D affine part | Unchanged from ADR-0013 D2, but now applied in geometry *and* paint through the same function — so the two agree even where both approximate. Exact for the `translate3d(x, y, 0)` / `translateZ(0)` compositing hints that dominate real pages. |
| **No stacking-context tree** | Hit-test ordering remains ADR-0006 §7's `(priority, z-index, index)` approximation; neither `transform` nor `opacity` establishes one for ordering purposes. Transform inversion is orthogonal to it, and conflating the two would double the change. |
| **A sub-`rect` for `scroll_into_view_if_needed`** is offset within the element's *visual* bounding rect | So it is approximate on a rotated element — as is everything expressed in axis-aligned rects there. |
| **No `@media print`, no relayout at paper width** | Wide content is fit-to-width scaled instead. A real print stylesheet needs a second style resolution against a print medium, which is a stage of its own. |
| **No CSS fragmentation properties** (`break-*`, `orphans`/`widows`), no header/footer templates, no tagged PDF, no WebP screenshots | The roadmap's own non-goals for this stage. Forced breaks are a small extension of the break list (a flagged opportunity the fill must take). |
| **Page count is capped** at `MAX_PDF_PAGES` (1000) | In the spirit of the engine's other budgets: a pathological document must not produce an unbounded file. The final page runs to the end of the document rather than truncating content. |
| **A hoisted out-of-flow box is still clipped by its DOM ancestors' overflow** | ADR-0013's third limit, untouched. |
