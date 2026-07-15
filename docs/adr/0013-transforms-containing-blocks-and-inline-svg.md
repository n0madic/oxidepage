# ADR-0013: CSS transforms, real containing blocks, inline SVG, and DOM paint order

- Status: accepted
- Date: 2026-07-11

## Context

With ADR-0012 the target SPA's Angular app finally *ran*, and the rendering could be
compared against a real browser for the first time. It was badly wrong: the
off-canvas menu sat on top of the hero, the header was half-covered, images and
logos were missing, sections overlapped, and the footer was painted under the
first screen. A side-by-side against headless Chrome (computed styles pulled over
CDP, so the comparison is against what the browser actually resolved, not a
guess) traced all of it to six engine-level gaps — none Angular-specific, all
things any modern page relies on.

## Decisions

**D1 — SVG and WebP decoding are on by default.** They already existed behind the
`svg` / `webp` cargo features of `oxidepage-paint`, and nothing turned them on:
22 of the target SPA's 48 images are SVG. A renderer whose logos silently vanish is not
useful, so `default = ["svg", "webp"]`; embedders who want the smaller build opt
out with `default-features = false`.

**D2 — `transform` is applied at paint time, not in layout.** `convert::transform`
resolves the computed transform list about `transform-origin` (percentages
against the border box) into the `Transform2D` that `DisplayItem::PushLayer`
has always carried and nobody ever filled in. The rasterizer keeps a CTM stack
(saved/restored per layer) and maps every draw, clip mask, and layer bound
through it; the PDF backend emits the equivalent `cm`. A 3D list is flattened to
its 2D affine part — exact for the `translate3d(x, y, 0)` / `translateZ(0)`
compositing hints that dominate real pages, an approximation for genuine 3D.

Layout still ignores transforms (ADR-0006 §7), which is what browsers do for
*layout* but not for `getBoundingClientRect`; the geometry APIs therefore still
report untransformed boxes (v1 limitation). Painting is where transforms are
load-bearing: without them every off-canvas panel (`translateY(-100%)`) and every
carousel (Swiper translates its slides) renders on top of the page — the target SPA's
document was 1509px wide instead of 1280px purely from untranslated slides.

**D3 — Out-of-flow boxes are laid out against their real containing block, and
painted where the DOM puts them.** Taffy has no containing-block chain: it lays an
absolute child out against its *direct parent*, which ADR-0006 §6 shipped as the
v1 approximation. `positioning::hoist_out_of_flow` re-parents each out-of-flow box
onto its actual containing block before layout — the nearest positioned ancestor
for `absolute`, the root for `fixed` — so taffy resolves its insets *and* its
percentage sizes against the right box.

Hoisting loses the *static position* (where an all-`auto`-inset box would have sat
in the flow, which taffy approximates with the parent's content origin), so the
box remembers the parent it was built under and
`positioning::restore_static_positions` puts it back after layout, per axis.

Paint order, however, must **not** follow the containing-block tree: CSS stacks a
positioned box among its DOM siblings. The target SPA's menu is `z-index: 100` inside a
header whose own controls are `z-index: 101` — painting the menu from its
containing block put it after the whole header subtree and blanked the header out.
So the painter walks the DOM tree (a hoisted box paints from the parent it was
built under, listed there as a `hoisted_child`) and takes each box's origin from a
precomputed table filled by walking the *layout* tree. That table also pins
`position: fixed` boxes to the viewport, which document scrolling now leaves alone.

**D4 — Text inside a positioned inline element paints with the positioned
descendants.** CSS 2.1 Appendix E paints in-flow inline content (step 5) before
positioned descendants (steps 6–8), and a `position: relative` `<span>` *is* a
positioned descendant. Sites lean on this: the target SPA underlines its section
headings with an absolutely positioned `::before` bar and keeps the words legible
by wrapping them in a relative span. An IFC is a single parley layout, so the two
slices are separated per glyph run by the run's owning node, and the positioned
slice is emitted from the nearest positioned ancestor — the run's IFC often lives
in an anonymous block several levels below the box whose `::before` it must beat.

**D5 — An inline `<svg>` is a replaced element, stored as vector source and
rasterized at the device size.**
Box construction already treated `<svg>` as replaced but sized it from a `src`
attribute it does not have, so every inline SVG was 0×0: on the target SPA that is the
logo and 44 icons. The page now serializes each `<svg>` subtree into the image
store under a key derived from the markup and the element's computed `color`
(`images::inline_svg_key`), which box construction looks up.

What is stored is the *source*, not pixels. An `ImageStore` entry is either
`ImageData::Raster` (decoded RGBA) or `ImageData::Vector` (SVG markup); both carry
the intrinsic size, which is all layout reads. The backends rasterize a vector
entry themselves, at the size it actually paints at: raster-skia derives the device
size from the CTM (so it folds in the device pixel ratio *and* any CSS
`transform`), export-pdf embeds it at 3× the destination rect (~288 dpi). An icon
with a `viewBox="0 0 24 24"` shown at 200px, or a logo at `dpr: 2`, is therefore
rendered at that resolution rather than resampled up from a 24×24 bitmap.
`oxidepage_paint::decode_image` only *parses* an SVG (for its intrinsic size);
`rasterize_svg` is the backends' entry point, and both go through the same hardened
`usvg::Options` that refuses to resolve an `<image href>` off the filesystem.

resvg renders the SVG in isolation and knows nothing of the surrounding cascade,
so the page embeds the element's computed `color` into the `style` attribute of the
markup it stores (`images::inline_svg_source`) — that is what makes
`fill="currentColor"` resolve to the inherited CSS color. It is *prepended*, so an
author's own `color:` in that attribute still wins. The color is part of the store
key precisely so that recoloring an icon produces a new entry instead of reusing
the old pixels. Identical icons still share one entry; a mutated `<svg>` gets its
own.

The cost of keying on size is a pixmap per (image, device size) — a vector icon
drawn at three sizes rasterizes three times. The cache lives for one `Canvas`, i.e.
one render, which is the whole workload for a headless engine whose output is
one-shot screenshots and PDFs. A cross-frame cache is a later question, not a
structural one.

**D6 — A percentage with no basis to resolve against is `auto`, in both places
taffy said zero.**

- *Replaced elements:* taffy probes intrinsic widths with the height's available
  space set to `MinContent`, and the CSS "replaced percentage min contribution"
  rule was applied to both axes on that signal. It only ever applies to the axis
  being asked for. Reading it on the cross axis zeroed a percentage height and
  then, via the aspect ratio, the width: the target SPA's hero image (`width: 100%;
  height: 100%`) laid out 0×0.
- *Blocks:* taffy folds `min-height` into the "style-based known size" a block
  passes down as its children's percentage basis. CSS does not — only a definite
  `height` does. `main` on the target SPA is `flex: 1 1 0%; min-height: 480px` wrapping
  a `height: 100%` element, so the entire page collapsed to 480px and the footer
  was painted under the hero. The minimum is now hidden from the inner pass and
  applied as what it is: a lower bound on the result.

## Consequences

The target SPA renders essentially as Chrome does at both 800×600 and 1280 full-page:
header, logo, hero image, carousels, world map, brand logos, case studies, cookie
banner, and a footer at the bottom. The document is no longer 18% too wide, and
the six fixes are all general CSS correctness — none of them mentions Angular.

New reftests (`transform-translate`, `abs-containing-block`,
`positioned-inline-stacking`, `inline-svg`, `inline-svg-scaled`) and layout tests
(`min_height_is_not_a_percentage_basis_for_children`,
`percentage_sized_image_in_auto_height_parent_uses_its_intrinsic_size`) pin the
behaviour; the reference geometry in the reftests was taken from Chrome.

## v1 limitations

- Geometry (`getBoundingClientRect`, hit-testing, scrollable overflow) still
  ignores transforms; only paint applies them. 3D transforms are flattened.
- A transformed box now establishes a containing block for its absolute *and*
  fixed descendants (`positioning.rs` walks ancestors with a transform-aware
  predicate), but `translate` / `rotate` / `scale` as *individual* properties
  are not read (only the `transform` list).
- A hoisted out-of-flow box is clipped by its DOM ancestors' `overflow`, not by
  its containing block's (CSS clips by the latter).
- SVG is rendered standalone. `currentColor` resolves (the computed `color` is
  embedded in the stored source), but nothing else inherits into it: font
  properties do not reach SVG text, and the CSS `fill` property (e.g. Bootstrap's
  `.bi { fill: currentColor }`) is not applied, so an icon whose symbol paths
  carry no `fill` of their own paints resvg's default black.
- Same-document sprite `<use href="#id">` / `xlink:href` references *are*
  resolved: `layout::images::inline_svg_markup` inlines the referenced
  `<symbol>`/definition into the isolated fragment (and declares `xmlns:xlink` +
  synthesizes a root `viewBox` so usvg accepts it), so the little
  `<svg><use href="#icon"></svg>` an icon sprite scatters through a page render
  instead of the broken-image square. External-file references
  (`<use href="sprite.svg#id">`) are still not fetched.
