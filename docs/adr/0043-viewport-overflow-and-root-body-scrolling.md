# ADR-0043: Viewport overflow, and which element scrolls the viewport

- Status: accepted
- Date: 2026-11-04

## Context

Three earlier choices left the document's scrolling different from every
browser's, and the WPT `css/cssom-view` files said so:

- **Only `scroll`/`auto` on `<body>` propagated to the viewport.** A `hidden`
  body kept its own clipping as "the conservative pre-existing behaviour". So
  `body { overflow: hidden }`, the scroll lock an open modal uses, clipped
  the document at the body's box, and the root's `scrollWidth` lost
  everything past it.
- **`overflow: hidden` or `clip` on the root pinned the document for script
  too.** `max_viewport_scroll` clamped those axes to zero, and a layout test
  pinned it as a regression fix. CSS Overflow says the opposite: `hidden`
  makes a scroll container that the *user* cannot scroll and a *program* can.
  This engine already scrolled a `hidden` `<div>` from script, so the
  viewport was the one inconsistent case.
- **Every element scroll member treated the root as the viewport, in both
  modes.** CSSOM-View splits that by compatibility mode. In standards mode
  the root stands for the viewport. In quirks mode the root scrolls nothing,
  and the body stands for the viewport unless it is *potentially scrollable*.
  `clientWidth`/`clientHeight` follow the same split. Only
  `document.scrollingElement` implemented it.

## Decision

1. **Propagation follows CSS Overflow §3.3.**
   - The viewport takes the root's `overflow`. On an axis where the root's
     is `visible`, the viewport takes the body's computed `overflow` instead,
     whatever its value, and the body's used `overflow` on that axis becomes
     `visible`.
   - The body's value is read from its computed style, not from its box. An
     earlier pass may already have rewritten the box, and the box survives an
     incremental reflow.
   - `LayoutEngine::propagate_body_overflow` remains the one place that
     decides this.

2. **Script scrolls the viewport whatever its `overflow`.** The viewport is
   always a scroll container: `visible` there means `auto`, and `clip` means
   `hidden`. So `set_viewport_scroll` clamps only to the scrolling area. The
   following all go through it:
   - `window.scrollTo`, `documentElement.scrollTop` and `scrollIntoView`;
   - the driver's own scroll.

   A **user** gesture goes through `LayoutEngine::scroll_viewport_by_user`
   instead. It refuses each axis where the propagated `overflow` is `hidden`
   or `clip`. A wheel from synthesized input is the one caller today; a
   future keyboard scroll must use it too.

3. **One classification answers "what does this element's scroll member
   address".** `bindings::imp::geometry_support::scroll_target` returns one
   of `Viewport`, `Element` or `Nothing`:
   - `Viewport` for the root in standards mode, and for the body in quirks
     mode when it is not potentially scrollable;
   - `Nothing` for the root in quirks mode;
   - `Element` otherwise.

   `scrollTop`/`scrollLeft` and `scroll()`/`scrollTo()`/`scrollBy()` use it.
   So does `scrollWidth`/`scrollHeight`: a body standing for the viewport
   reports the viewport's scrolling area. `scrollingElement` uses the same
   "potentially scrollable" test, so the two cannot disagree.
   `clientWidth`/`clientHeight` report the viewport for the root in standards
   mode and for the body in quirks mode, and the padding box otherwise,
   including the root in quirks mode.

4. **Scroll members act on the frame that renders the node.** This covers
   both the flush and the queued `scroll` event, so a parent script that
   scrolls `iframe.contentDocument.documentElement`:
   - moves the frame's viewport;
   - fires `scroll` at the frame's document.

   A frame's viewport is also sized from its `<iframe>` on demand
   (`fit_frame_viewport`, shared with the page's layout walk), so the frame's
   own script measures itself before the page has laid out.

## Consequences

- `body { overflow: hidden }` no longer cuts the document off at the body:
  - `scrollHeight`, `scrollWidth` and a `--full-page` capture see the whole
    document;
  - script can still scroll it, as in Chrome;
  - a wheel cannot.
- `<html style="overflow: hidden">` now scrolls from script.
  `overflow_hidden_root_scrolls_for_script_but_not_the_user` pins the
  user/script split that replaces the old pin.
- Pages in quirks mode change in two ways:
  - `documentElement.scrollTop` reads 0 and its writes do nothing;
  - `body.scrollTop` is the document's scroll, which is what such pages were
    written against.
- Wheel scrolling of a `hidden` *element* is unchanged: it still scrolls.
  Only the viewport has the user/script split for now.
- WPT: `scrolling-quirks-vs-nonquirks.html` passes in full, with gains in
  `scrollingElement.html`, `scrollintoview.html`,
  `scrollIntoView-root-overflow-clip.html` and
  `HTMLBody-ScrollArea_quirksmode.html`.
