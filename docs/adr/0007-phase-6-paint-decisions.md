# ADR-0007: Phase 6 paint, raster & PDF implementation decisions

- Status: accepted
- Date: 2026-07-05

## Context

Phase 6 (design doc §5.8–5.11) turns the laid-out box tree into pixels: a
backend-neutral **display list**, a **tiny-skia** CPU rasterizer, a **PDF**
exporter, an image pipeline (network → decode → intrinsic sizes → paint), the
HTML `requestAnimationFrame` + "update the rendering" step, and the test
scaffolding (Ahem reftests, display-list goldens). This ADR records the
decisions (D1–D8), the deviations from the design document, and the v1 limits.

## Decision

### D1. Glyph rasterization uses skrifa outlines, not swash

The design doc (§5.9) names swash for glyph rasterization. We already depend on
`skrifa 0.42` (via parley), so `paint::glyphs::glyph_outline` extracts **unhinted
outlines once** as a backend-neutral `PathCommand` list (y-flipped to screen
coordinates). `oxidepage-raster-skia` converts those to `tiny_skia::Path` (cached
per `(font, glyph, size)`), and `oxidepage-export-pdf` converts them to PDF path
operators. Unhinted outlines are deterministic across platforms — the reftest
requirement — and avoid a second font-rasterization dependency. parley 0.10's
`Run::font()` yields `FontData { data: Blob<u8>, index }`, from which
`skrifa::FontRef::from_index` builds the face.

### D2. Paint styles are read at paint time, not captured at construction

`dom.primary_style(node)` / `dom.pseudo_style(node, …)` work **without** an
`enter_active_tree` scope (they clone the stored computed-value `Arc`; they never
create `NodeRef`s), so the paint walk reads background/border/color/opacity/
visibility directly from stylo at paint time. `LayoutBox` is not enlarged with
paint fields. `visibility: hidden` skips a box's own paint but descends into
children; `opacity: 0` skips the subtree; `0 < opacity < 1` emits a `PushLayer`.
CSS `transform` is deferred: `PushLayer` keeps a `Transform2D` field (always
identity in v1).

### D3. PDF text is glyph outlines (paths), not embedded fonts

Text in the PDF is painted as filled vector outlines (D1), geometry-identical to
the raster backend, with **no font dictionary**. Font embedding + subsetting
(making PDF text selectable/extractable) is Phase 7. The v1 tests assert the
contract: a glyph run produces path operators and no `/Font`.

### D4. New dependencies

`tiny-skia 0.12` (CPU raster), `png 0.18` (PNG encode), `image 0.25`
(PNG/JPEG/GIF decode; WebP behind the paint `webp` feature), `resvg 0.47`
(SVG decode behind the paint `svg` feature), `pdf-writer 0.15` (PDF byte
stream). The display-list JSON dump is a **hand-written writer** (floats to two
decimals, `-0.00` normalized) — no serde — so goldens are byte-stable and
reviewable. Fonts and images are referenced in the JSON by their **resource
ordinal**, not their raw id, because both ids come from per-process counters
(`Blob::id()`, the image store counter) that vary run to run.

### D5. Stacking contexts — a pragmatic subset of CSS 2.1 Appendix E

The paint walk orders each box's children back-to-front into four buckets:
positioned `z < 0`, in-flow, positioned `z: auto/0`, positioned `z > 0`;
ties break by z-index then tree order (the same order the layout hit-test uses).
Subtrees paint atomically per box rather than fully re-threading positioned
descendants into their nearest ancestor stacking context — a v1 simplification
that is correct for the common cases (z-order, nested opacity, overflow clip)
and matches `elementFromPoint`. `overflow != visible` wraps a box's content
(steps 3–7) in a padding-box clip with border-adjusted radii; scrolling subtracts
the scroll offset from child origins. The captured `LayoutBox::z_index` collapses
`auto → 0` (kept for the hit-test); paint reuses it directly since `auto` and `0`
paint in the same step.

### D6. The display-list cache lives on `Page`, keyed by a paint stamp

`Page` holds a `RenderState { cache, stamp }`; the list is rebuilt only when the
`PaintStamp { dom_version, style_version, viewport, element_scroll_version,
images_version, fonts_version }` changes. Scrolling dirties paint but not layout
(design §5.11), so `ScrollState` counts scroll offsets, and the layout engine
carries an `images_version` (bumped by the image store). A decoded image changing
intrinsic sizes forces a full box-tree rebuild (the incremental patch bails on an
image-version change).

The *document* (viewport) scroll is deliberately absent from the stamp: it is
applied by the rasterizer as a translate, not baked into item origins, so the
cached list is reused across scroll positions (a scroll no longer re-walks the
box tree). `position: fixed` subtrees are wrapped in `PushViewportAnchor` /
`PopViewportAnchor` markers so the rasterizer leaves them pinned. Element
overflow scroll *is* baked into origins and so stays in the stamp
(`element_scroll_version`).

### D7. Backgrounds and borders

Background painting emits the `background-color` fill (border box, element
radii) then one fill/image per background layer, bottom-to-top. Gradients:
`linear-gradient` (angle/keyword/corner → endpoints), `radial-gradient`
(circle/ellipse × closest/farthest-side/corner → center + radii), multi-stop
with implicit-position normalization; `background-size` (`auto`/`cover`/`contain`/
explicit, aspect-aware for images), `background-position` (length/%),
`background-repeat` (`repeat`/`x`/`y`/`no-repeat`; `space`/`round` fold to
`repeat`). The canvas background is propagated `html → body` over an opaque-white
base. Borders: every style except `none`/`hidden` rasterizes as `solid`; uniform
borders are a rounded ring (even-odd), mixed borders are per-edge trapezoids
(rectangular). **Deferred**: `background-origin`/`background-clip` keywords
(v1 uses padding-box origin, border-box clip), conic gradients,
`background-attachment`, blend modes, `background-image: url()` tiling with an
explicit smaller size than the area (tiled via a raster pattern shader across the
current clip; `RepeatX`/`RepeatY` fall back to full repeat), and PDF image tiling
(drawn once).

### D8. rAF + "update the rendering" in the headless loop

`HostHooks` gained `request_animation_frame`/`cancel_animation_frame`;
`requestAnimationFrame`/`cancelAnimationFrame` are installed on the global by the
timer pattern, with callbacks held in `LoopHooks`. A **rendering opportunity**
fires when animation-frame callbacks are pending and `now >= next_render_at`
(16 ms cadence); `run_until_stalled` treats it as another progress source and
`settle` folds `next_render_at` into its blocking-wait deadline and exit
condition, so an endless rAF loop is bounded by the settle budget (like
`setInterval`). `update_the_rendering` swaps the callback list, fires each with
the elapsed-ms timestamp, runs a microtask checkpoint, flushes layout, and — when
a consumer has asked for output — refreshes the cached display list.
`display_list()`/`screenshot()`/`print_to_pdf()` force one step. Image decoding
runs **synchronously on the page thread** at `NetEvent::Done` (a deviation from
the design's off-thread decode pool, §5.10 — post-v1); `data:` URLs decode
inline. Images participate in `in_flight`, so the `load` event waits for them.
PDF is a single page sized `viewport.width × max(content_height, viewport.height)`
in CSS px, converted to points (× 0.75). Because that page spans the whole
document, PDF export paints from the document's top-left and ignores the
viewport (document) scroll (`build_display_list_full`), so scripted scrolling
never shifts or clips it; element `overflow` scroll offsets still apply. A
**full-page screenshot** (`screenshot_full_page`, `screenshot --full-page`) uses
the same unscrolled display list, rasterized over `content_size` rather than the
viewport; the rasterizer's device-size caps clamp absurdly long documents.
`background-image` loading scans element **and** `::before`/`::after` styles.

## Consequences

- New crates `paint`, `raster-skia`, `export-pdf` are live; `raster-vello`
  remains a stub (GPU backend is post-v1, but the display-list boundary means it
  is a backend, not a rewrite).
- CLI gains `dump-display-list`, `screenshot --dpr [--full-page]`, and `pdf`;
  `Page` gains `display_list`, `render_pixels`, `render_pixels_full_page`,
  `screenshot`, `screenshot_full_page`, `print_to_pdf`.
- `cargo xtask golden [--update] [--filter]` compares display-list JSON goldens;
  `cargo xtask reftest [--filter]` pixel-compares an Ahem reftest corpus with WPT
  `fuzzy` tolerances. Both run in CI across the three-OS matrix.
- Determinism holds for a fixed font set (unhinted outlines, Ahem bundled),
  which the reftests and goldens depend on.

## v1 limitations (summary)

CSS transforms; `background-origin`/`clip` keywords; conic gradients;
`background-attachment`/blend-mode; true tiling of explicitly-sized background
images and `RepeatX`/`RepeatY` (folded to full repeat); PDF font embedding
(text is outlines) and PDF image/gradient fidelity (images drawn once, gradients
approximated by their end stops); off-thread image decoding.

> A block-level replaced element with `width: auto` is sized to its intrinsic
> width (CSS 2.2 §10.3.4) by dropping the container width taffy's block
> algorithm would stretch it to; the drop is gated on the parent being a block
> container, so flex/grid items still grow/shrink. Inline replaced elements are
> sized and positioned by the inline formatting context, which writes their
> geometry back into the box tree's `unrounded_layout` (rounded into
> `final_layout` by taffy's rounding pass), so `getBoundingClientRect` and paint
> see the correct rects.
