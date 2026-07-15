# ADR-0014: Viewport-driven lazy image loading

- Status: accepted
- Date: 2026-07-11

## Context

The engine fetched every image on the page, always: an `<img>` starts its load
when it connects to the DOM, and a scan of computed styles starts one for every
`background-image`. Nothing read `loading="lazy"`, and the CLI's own comments
said as much — "a headless engine has no lazy-loading, so *every* image is
fetched" — which is why its default byte budget had to be raised to 512 MiB.

For a viewport screenshot that is almost all waste. The target SPA pulls hundreds of
megabytes to paint the handful of images that are actually on the first screen.

The infrastructure to do better was already in the tree and already load-bearing
for `IntersectionObserver`: viewport-relative geometry
(`bounding_client_rect`), a version-counter gate over the resource scans, and a
place in the event loop where such scans run.

## Decisions

**D1 — Lazy loading is a page option, off by default.** `PageOptions.lazy_images`
means "fetch an `<img>` only once it reaches the viewport plus one screen of
margin". Off by default: an embedder that asked for a page got the whole page,
and goldens, reftests and the existing suites must keep the behaviour they pin.
The CLI turns it on for exactly one command — a *viewport* `screenshot` — since
that is the only output that does not depend on the rest of the document.
`--lazy-images` / `--no-lazy-images` override that per invocation.

**D2 — Deferral happens before the URL dedup, not inside it.**
`start_image_load_url` claims the URL in `requested_images` on its first line;
that set is what makes a URL "already handled". Deferring inside it would mean
the image is never fetched at all. So the deferral is at the *node* level, in
`start_image_load`, and the queue holds `NodeId`s — not URLs. The `src` is
resolved when the load actually starts, because it can change while an image
waits. A deferred image that later becomes visible goes through the same
`start_image_load_url`, so two `<img>` with one `src` still cost one fetch.

**D3 — `data:` and `loading="eager"` are never deferred.** A `data:` URL decodes
inline and costs no network, so deferring it buys nothing; `loading="eager"` is
the author saying exactly what to do. In eager mode `loading="lazy"` is ignored
outright — an embedder that did not ask for lazy loading gets everything. In
lazy mode the attribute does not *enable* laziness (it is already on), it tunes
the margin: `lazy` → no margin, anything else → one viewport.

**D4 — The visibility gate is four live version counters plus the paint stamp,
not the paint stamp alone.** The scan runs once per event-loop iteration, gated
on `(dom.style_version, dom.structure_version, style.version,
layout.paint_stamp, layout.document_scroll_version)`. Both of the additions to
`PaintStamp` are load-bearing:

- The stamp's `style_version` is the one recorded at the *last reflow*, not the
  live one. An external stylesheet landing without a DOM mutation bumps only
  `style.version()`, and nothing else in the loop reflows — so a first pass that
  ran against the pre-CSS layout would never re-run, and an image the sheet
  reveals on the first screen would be a hole in the screenshot. This is the
  same trap `start_background_image_loads` and `start_font_face_loads` already
  document.
- `PaintStamp` excludes document scroll *on purpose* (the rasterizer applies it
  after the display list), so `window.scrollTo` would not reopen the gate.

All five are reads, so a shut gate costs nothing. Like the sibling resource
scans, the pass never reports progress to the loop — progress arrives as net
events from the loads it starts, and claiming it here would spin the loop.

**D5 — Convergence is the loop's job, not the scan's.** A load changes the
image's intrinsic size, which bumps `images_version`, reflows, and moves
everything below it — which may bring new images into view. That is a net event,
so the loop iterates, the stamp has changed, the gate opens, and the next pass
picks them up. An empty pass shuts the gate again. The one-viewport margin
absorbs most of the churn on pages whose images carry no dimensions.

**D6 — Freed nodes are dropped before geometry is touched.** `DomTree::node()`
panics on a stale `NodeId`, and `bounding_client_rect` calls it. Unlike the
drain queues, deferred nodes wait indefinitely, so an SPA that removes an
`<img>` leaves a stale id behind — the same hazard `IntersectionObserver` guards
its targets against, guarded the same way (`retain(|n| dom.get(n).is_some())`).

**D7 — Intersection is inclusive.** An `<img>` with no `width`/`height` and no
image yet lays out 0×0, and a zero-area rect never *overlaps* anything. A strict
test would defer such an image forever — and it would never load to gain the
size that would undefer it.

**D8 — Full-page output must undefer explicitly.** `Page::load_deferred_images`
starts everything still queued, waits for it, and leaves the page eager. It is a
required step before `screenshot_full_page` / `print_to_pdf` on a lazy page; the
CLI calls it on both paths. Cheap insurance, and better than hiding network I/O
inside a render call.

## Consequences

A viewport screenshot fetches the images it can show plus one screen of margin,
instead of every image in the document. `window.load` is not held back by a
deferred image (which is also what the spec says of lazy images), and `settle`
still waits for every load that *did* start, so output is never taken mid-fetch.

`dump-display-list` stays eager even though it prints the same viewport display
list a `screenshot` rasterizes. The asymmetry is deliberate — the command exists
for debugging and goldens, which want the whole list — and is called out in the
CLI usage so that "the dump and the screenshot of one document differ" does not
read as a bug. `--lazy-images` gets the screenshot's view.

Pinned by `crates/page/tests/lazy_images.rs` (15 tests), including a regression
for each hazard above: the external sheet reopening the gate (D4), a removed
deferred image (D6), the shared URL dedup (D2), and a navigation clearing the
queue.

## v1 limitations

- **`background-image` is still eager.** Its scan is gated on
  `(dom.style_version, style.version)` — no scroll — so a deferred background
  that scrolled into view would never load. Doing it right means keying the scan
  by node, a second dedup, and rects for pseudo-element owners: a feature of its
  own. The bytes on a content page are in `<img>` anyway.
- No `srcset` / `sizes` / `<picture>` (the engine has none); laziness reads `src`.
- Images come in on programmatic scroll (`window.scrollTo`) — the only scroll
  there is.
- Web fonts and inline SVG are not lazy (the latter rasterizes locally).
- An image that scrolls back out of view is not evicted; its bytes live until
  the next navigation.
- Background images inside shadow trees are not loaded at all (the scan walks the
  light tree) — not a regression, but true.
- Lazy mode puts a reflow in the event loop: every completed image bumps
  `images_version`, which forces a full box-tree rebuild, and the scan reflows to
  read geometry. On a page with no observers or rAF, the eager path reflows in
  the loop not at all. Completions coalesce (the loop drains all ready net events
  per iteration), and the geometry is needed regardless — but it is not free.
