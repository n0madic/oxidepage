# ADR-0015: Whole-document visibility for full-page output, and no more silent failures

- Status: accepted
- Date: 2026-07-11

## Context

Rendering `https://vuejs.org` surfaced three unrelated script errors and a page
that was missing most of its content. Chasing them down turned up one theme: the
engine failed *quietly*, and each quiet failure hid the next.

`<script type=module>` reported `module ... evaluation rejected` and nothing
else — the rejection reason was discarded at the point it was detected. Behind
that were two more silent paths: a rejected promise with no handler was dropped
on the floor entirely (nothing in `page` ever installed the rejection tracker
the `js` crate already exposed), and a *negative* brand check poisoned the JS
context — rquickjs's `instance_of` bottoms out in `JS_GetOpaque2`, which throws
a `TypeError` into the context on a miss and then reports the miss as a plain
`false`, so `host_payload` left a stray "RustClass object expected" pending,
which the next promise job picked up and blamed on unrelated script.

Underneath those, the page's real breakage was a chain of missing brand-check
globals, each one a `ReferenceError` in code that had no reason to guard:
`link.relList.supports()` (implemented as an unconditional throw),
`document.referrer` (absent), `localStorage instanceof Storage` (Web Storage was
implemented, but `Storage` itself was not installed and the storage objects were
plain objects), and `el instanceof SVGElement` in Vue's `createApp().mount()`.
The last of these is what kept the app from mounting at all.

With the app finally mounting, one gap remained: a sponsor grid that Vue renders
only once an `IntersectionObserver` reports it visible stayed empty in
`--full-page` output. The implicit intersection root is the viewport, the
document never scrolls in a headless engine, and so everything below the first
screen is permanently "not yet seen" — even though all of it is in the image
being produced. This is precisely the failure ADR-0014 already solved for
`<img loading=lazy>`, in its script-driven form.

## Decisions

**D1 — Full-page output makes the whole document the intersection root.**
`PageOptions.whole_document_visible` grows the *implicit* (viewport) root of an
`IntersectionObserver` to the document's scrollable rect. It is off by default,
because the spec says the root is the viewport and WPT tests exactly that; the
CLI turns it on for the two outputs that are the whole document — `--full-page`
screenshots and `pdf`. An explicit element root is untouched: it remains its own
padding box. The reasoning is the same as ADR-0014 D1: an embedder that asks for
the whole document is asking for the whole document, and content the page gates
on visibility is content, not an optimization.

**D2 — An unhandled promise rejection is a reported error.** `Page` installs the
rejection tracker and holds each rejection until `drain_errors`, dropping the
ones that acquire a handler in the meantime (a handler may attach turns later,
so reporting at rejection time would cry wolf). Browsers surface these on the
console; a headless engine has no console for anyone to notice, and every bug in
this ADR's context section was invisible for exactly that reason.

**D3 — A brand check must not disturb the JS context.** `host_payload` now
parks any pending exception, runs the check, swallows the `TypeError` the check
itself raised on a miss, and restores what it parked. A miss is an ordinary
answer — the bindings probe values that come straight from script — and must
cost nothing.

**D4 — Interface objects are installed only where we implement them.** P6 says
absent beats fake, and the fix for a missing brand-check global is to *implement
the thing*, not to install a hollow constructor so `instanceof` stops throwing:

- `DOMTokenList.supports()` answers for `rel` (whose supported tokens the HTML
  spec defines) and still throws for `class`/`part` (which define none). It
  reports only the link types the engine acts on — `stylesheet` — so
  `supports('modulepreload')` is an honest `false`, and Vite's polyfill takes
  its fallback path instead of dying on a `TypeError`.
- `document.referrer` returns `""`, which is not a stub but the correct value:
  every navigation here is a top-level `NetRequest::navigation`, defined
  no-referrer, and `Location` has no setter for script to navigate with.
- `Storage` becomes a real interface whose members live on the prototype (script
  monkey-patches `Storage.prototype.setItem`), with `localStorage` /
  `sessionStorage` as instances of it. `StorageEvent` is constructible because
  script constructs it; the engine never *fires* one, which is also correct — a
  storage event notifies the *other* documents of an origin, and there are none.
- `SVGElement` backs every element in the SVG namespace, and `SVGAElement.href`
  is a real `SVGAnimatedString` reflecting the `href` (or legacy `xlink:href`)
  attribute, with `animVal == baseVal` because no SMIL animation is in effect.
  The rest of the SVG DOM stays absent.

## Consequences

`drain_errors` now reports things it never used to, which is the point: a page
whose bootstrap chain rejects says so instead of rendering a blank shell. An
embedder that treats any reported error as fatal will see failures it was
previously blind to.

`whole_document_visible` is a deliberate, opt-in deviation from the
IntersectionObserver spec, and the second option (after `lazy_images`) whose
correct setting depends on what the embedder intends to *do* with the page. Both
are off by default, so conformance runs and library embedders are unaffected.

CSS multi-column layout remained unimplemented as of this ADR (stylo parsed
`column-count`; nothing in `layout` consumed it), so a multicol container laid
out as a single column — visible in vuejs.org's footer. That was a layout
feature, not a binding gap, and was left to its own change: ADR-0016 implements
it as clipped, translated views of one continuous flow.
