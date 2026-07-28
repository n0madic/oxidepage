# ADR-0028: `new Image()` — legacy factory functions, and pending activity as a pin

- Status: accepted
- Date: 2026-07-28

## Context

`Image` is a WebIDL `[LegacyFactoryFunction]` on `HTMLImageElement` — a second,
differently named constructor for an interface that already exists. The codegen
had no concept of the annotation, and interface-level extended attributes were
not read at all, so the construct could not even be spelled in the IDL. The
interface itself has been complete since Phase 2; only its factory was missing.

`new Image()` is not a corner of the platform. It is *the* image preload idiom,
and the way feature detection probes a codec ("does this browser decode WebP?"
is a `data:image/webp` URL and an `onerror`). Fixing the `ReferenceError` alone
surfaced two further reasons the idiom did not work, both of which had been
invisible while `Image` was undefined:

1. **Resource loading gated on `IS_CONNECTED`.** A `new Image()` never enters
   the tree, so `img.src = …` queued nothing and no load ever started.
2. **A detached `<img>` with a load in flight was garbage.** Written the usual
   way — `const i = new Image(); i.onload = …; i.src = …;` inside a function —
   the element is unreachable from JS the moment that function returns. The
   image update queue is drained a *later* turn of the event loop, and that turn
   processes finalized wrappers first, so a GC in between freed the node and the
   load silently never happened. html5test hung on exactly this: four codec
   probes (`canvas.webpLoad`, `canvas.jxl`, `canvas.avif`, `canvas.heic`) whose
   `load`/`error` never arrived, so the suite spun on its "background tasks
   still running" poll and never rendered a score.

## Decisions

**D1 — `[LegacyFactoryFunction]` is a codegen construct, not a bootstrap
shim.** `crates/idl` now reads interface-level extended attributes and emits, per
factory, a glue function calling `imp::<interface>::factory_<name>`. Writing
`Image` by hand in `bootstrap.js` would have been shorter and would have put a
platform constructor outside the one place that knows what the platform exposes;
the annotation is also the only thing that makes `Audio` and `Option` a
declaration each rather than a design each.

Interface-level extended attributes were previously ignored wholesale. They are
now parsed, and anything other than `[LegacyFactoryFunction]` is a **build-time
error** — the same rule members have followed since Phase 2, for the same
reason: an annotation we neither honor nor recognize is a silent behavior gap.

**D2 — A legacy factory is not an interface object.** `JsScope` grows
`new_legacy_factory` alongside `new_constructor`; they share one implementation
and differ in one line. `Image.prototype === HTMLImageElement.prototype`, but the
factory does **not** claim `proto.constructor` (which keeps naming
`HTMLImageElement`) and is absent from `PageState::interfaces`, so brand checks
and `this`-unwrapping are untouched. An interface may carry more than one
factory; only one of them could ever own the back-reference, so none does.

**D3 — Image loading gates on the *node document* plus template inertness, not
on `IS_CONNECTED`.** HTML runs "update the image data" on any `src` change,
connected or not. The check in `note_style_owner_attr` and in `start_image_load`
becomes `node_document(el) == dom.document() && !in_template_contents(el)`.

`IS_CONNECTED` was standing in for two separate rules, and both have to be asked
for by name once it goes:

- A document with no browsing context (`DOMParser`, `createHTMLDocument`,
  `new Document()`) loads nothing — ADR-0017, and the node-document check.
- A `<template>`'s contents load nothing. HTML puts them in a separate "template
  contents owner document"; the engine reuses the node document
  (`create_template_contents` takes the host's), so the node-document check
  alone would happily fetch an image the page never displays — and worse, the
  load would join `in_flight` and hold up `settle`, the `load` event and every
  screenshot. `DomTree::in_template_contents` walks to the tree root and asks
  whether it is a fragment with a host and no shadow mode, which is exactly a
  template contents fragment.

This is a deliberate, narrow exception to "resource loading gates on
`IS_CONNECTED`". It applies to images and to nothing else — the flag still means
"in the rendered document", and every other consumer still asks it.

**D4 — A detached image is eager.** Lazy deferral (ADR-0014) is keyed on
intersecting the viewport, which a node outside the tree can never do. Deferring
a detached image is not deferring it, it is dropping it, so `start_image_load`
treats detachment as `loading="eager"`.

**D5 — Pending activity pins the node, in two hand-offs.** HTML says an `img`
with pending activity is not collected; here that is a pin, and the window it
must cover spans two different owners:

- `push_image_update` pins, and the drain releases — this covers the gap between
  the `src` assignment and the drain, which is where the GC actually struck.
- `register_image_waiter` pins, and `notify_image_waiters` releases after
  dispatching `load`/`error` — this covers the load itself.

The drain releases the queue pin **after** `start_image_load` returns, never
before: the settled path fires an event, and a GC inside that callback would
find the node unpinned. Both releases go through `release_image_pin`, which
retries `free_detached_tree_if_unpinned` under the same guard the wrapper
finalizer uses — this pin may well be the last one, and nothing else would come
back to collect the node.

**A node waits at most once, and a pin is 1:1 with a wait.** Both halves are
load-bearing:

- One wait per node, or one load fires two events. `el.src = …` on a detached
  `<img>` followed by `appendChild` queues the update *twice* — once for the
  attribute, once for the connection — and a blind `push` into
  `image_waiters[url]` put the node in the list twice. `requested_images` still
  dedupes the fetch, so only the event doubles, which is exactly what breaks the
  counting idiom (`if (++loaded === total) done()`) every preloader is built on.
  `register_image_waiter` drops the node's earlier waits before adding its own.
- A pin per wait, or a `src` reassigned mid-flight leaks one. The first wait is
  orphaned — nothing fetches that URL a second time, so its
  `notify_image_waiters` never comes — and `unregister_image_waiter` is what
  releases it.

**A deferred image holds no wait and no pin.** Lazy deferral parks a *node*, and
a deferred node is connected by construction (D4), so it cannot be collected
anyway; a pin taken there would be released only by a fetch that, by definition,
has not happened. That would make the scan's own liveness sweep
(`deferred.retain(|n| dom.get(n).is_some())`) dead code and let an SPA churning
lazy `<img>`s grow the arena without bound. So the wait starts where the load
does: `begin_image_load` is the single point that fires the settled event or
registers-and-fetches, and both `start_image_load` and the visibility scan go
through it.

`reset_for_navigation` clears `image_waiters` with the other node-keyed queues.
The pins go away with the arena; the entries would otherwise be stale ids.

## Consequences

`new Image()` works, including the preload idiom that keeps no reference to the
element.

The `[LegacyFactoryFunction]` machinery is general: `Audio` and `Option` are now
one IDL line plus one `imp` function each. Neither is implemented here —
`new Option(text, value, …)` builds a text node and has selection semantics of
its own, and there is no `HTMLAudioElement` interface yet.

`optional unsigned long` with no default is now expressible (`arg_opt_u32`),
which is what lets `new Image(32)` set `width` and leave `height` unset rather
than writing `height="0"`.

The pin is reference-counted like every other, so an image whose load never
completes holds one until navigation replaces the arena. That is the same shape
as any in-flight resource and is bounded by the page's request budget — the
deferred case, where no request is ever made, holds none at all.

A lazy image now also takes the settled fast path and gets its `load` event: the
visibility scan used to call `start_image_load_url` directly, registering no
waiter, so a deferred image that reached the viewport fired nothing. Routing it
through `begin_image_load` fixes that as a side effect of putting the wait and
the fetch in one place.

Pinned by `crates/page/tests/images.rs`: identity and construction
(`Image.prototype`, `HTMLImageElement.prototype.constructor`, omitted arguments,
calling without `new`), a load started from a factory-built element, the GC
window (a function-local image whose events must still fire), one event for
`create → src → append` and for a second queued update, ADR-0017's rule (an
`<img>` in a `DOMParser` document loads nothing) and template inertness; and by
`crates/page/tests/lazy_images.rs`, where a removed deferred image must be
collectable.

## v1 limitations

- Only `Image` is declared. `Audio` and `Option` are declarations away, but
  their interfaces are not here yet.
- A legacy factory's arguments are the IDL's; the spec's extra step for
  `Option` (creating a child text node) has no counterpart in the emitted glue
  and would live in its `imp`, as `factory_image`'s attribute writes do.
- The pin covers `<img>` loads only. Other resources with pending activity
  (`<script>`, `<link>`) are connected by construction, so nothing else needed
  it — but the pattern is not generalized.
