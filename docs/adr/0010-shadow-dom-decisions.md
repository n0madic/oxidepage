# ADR-0010: Shadow DOM (v1)

- Status: accepted
- Date: 2026-07-11

## Context

Custom elements (ADR-0009) unblocked component registration, but component
*rendering* still failed: web-component constructors (Swiper's
`swiper-container`/`swiper-slide` on a real-world ad-tech SPA, Lit, Stencil) call
`this.attachShadow({mode:"open"})` first thing. With `attachShadow` missing,
the constructor threw, the element was marked `Failed`, and the component
never built its markup — which lives entirely inside a shadow tree, with
slotted light-DOM projection and shadow-scoped CSS (`:host`, `::slotted()`,
`::part()`).

This ADR records the v1 Shadow DOM implementation: `attachShadow`/
`ShadowRoot`, `<slot>` projection, the flat tree in style traversal and
layout, shadow-scoped cascade through stylo's native support,
`adoptedStyleSheets` + constructable `CSSStyleSheet`, shadow-aware
`getRootNode`, and a composed event path.

## Decisions

**D1 — A shadow root is a `DocumentFragment` node modeled like template
contents, but participating.** `NodeData::DocumentFragment` gained
`shadow: Option<ShadowMode>`; the link is bidirectional
(`ElementData::shadow_root` ↔ fragment `host`). Unlike `<template>` contents,
the shadow tree participates in connectedness (composed propagation from the
host), style, layout, and the composed event path. Every place that
special-cases template contents (`free_subtree`, `subtree_has_pins`,
`tree_root_via_host`, `is_host_including_inclusive_ancestor`) handles
`shadow_root` the same way. Shadow nodes live in the same arena; no
cross-tree handles arise.

**D2 — The flat tree has one authoritative implementation:
`DomTree::flat_tree_children` / `flat_tree_parent`.** A host's flat children
are its shadow root's children; a `<slot>`'s flat children are its assigned
nodes (elements match by `slot` attribute against the slot `name`; text nodes
only ever match the default slot) or its own children as fallback; unassigned
light children of a host vanish. Both consumers — box-tree construction
(every child-walk site in `construct.rs`, including both IFC phases and the
table walk) and stylo's restyle traversal (`traversal_children` /
`traversal_parent` / `inheritance_parent`) — call it. The stylo `Traverser`
materializes the child list into a `Vec` (slot jumps are not expressible as
sibling links). Paint needed no changes: it walks the box tree.

**D3 — Shadow-scoped cascade via stylo's native `AuthorStyles`, with the
flushed `CascadeData` stored in a DOM side-map.** Shadow `<style>`/`<link>`
sheets never enter the document `Stylist`: `StyleEngine::add_sheet_for_node`
routes them (by `containing_shadow_root`) into a per-root `ShadowScope`
(tree-ordered node sheets + adopted sheets). Each `resolve_styles` flushes
dirty scopes (`AuthorStyles::flush`, rebuilding the whole sheet set on change
— v1 simplicity, the flush recomputes fully anyway) and writes the resulting
`Arc<CascadeData>` into `DomTree::shadow_cascade`, which
`TShadowRoot::style_data` reads through the active-tree thread-local.
`:host`, `::slotted()` and `::part()` then work through stylo's own rule
collector, given the element hooks (`containing_shadow`, `shadow_root`,
`assigned_slot`, `is_html_slot_element`, `has_part_attr`, `each_part`,
`is_part`). `@font-face` inside shadow scopes still registers fonts globally.

**D4 — Slot-assignment changes are invalidated explicitly (the Gecko
approach).** stylo propagates restyle hints only through elements that
already carry cascade data; a freshly created `<slot>` (e.g. via
`shadowRoot.innerHTML`) has none, so a hint on the host dies before reaching
already-styled assigned nodes. `note_slot_assignment_changed(host)` posts a
subtree restyle hint directly on every styled light child of the host; it
fires from `attachShadow`, from insert/remove of subtrees containing a
`<slot>` inside a shadow tree, and from `slot`/`name` attribute changes.

**D5 — Dirty/restyle chains cross the boundary via the composed parent.**
`mark_dirty_ancestors` and `note_stylo_restyle` walk `composed_parent`
(fragment → host), otherwise mutations inside a shadow tree would be
invisible to the next restyle. For the same reason `try_patch` bails to a
full box-tree rebuild whenever shadow roots exist: the incremental snapshot
walks the light tree only and would declare stale layouts valid.

**D6 — Shadow ids are scoped.** The document `getElementById` index refuses
elements with a containing shadow root, both on the connectedness walk and
on `id` reindexing; `ShadowRoot.getElementById` works through the
DocumentFragment path (subtree walk).

**D7 — `adoptedStyleSheets` + constructable `CSSStyleSheet`.**
`new CSSStyleSheet()` builds an owner-less sheet (`SheetData::Constructed`);
`replaceSync` swaps the sheet's rules **in place** under the shared lock so
every adopting scope follows, then conservatively dirties all author data.
`adoptedStyleSheets` exists on both `ShadowRoot` and `Document` (style
injectors feature-detect it; it defaults to an empty array) and is an
ObservableArray stand-in: a Proxy over a plain array whose in-place
mutations (`push`, indexed writes, `length` truncation, delete) re-validate
the entries and re-route them into the target scope
(`StyleEngine::set_adopted_sheets`), exactly like full reassignment. The
proxy is stored per node in `PageState`. Document adopted sheets order after
all node sheets in the `Stylist`.

**D8 — Composed event path, no retargeting.** `dispatch_event` builds the
path by crossing shadow root → host when `event.composed` is true and stops
at the shadow root when it is false; `composedPath()` reflects it.

**D9 — WebIDL fallout found by the Swiper e2e.** Indexed-getter interfaces
without `iterable<>` (`NamedNodeMap`, `HTMLCollection`) now expose
`@@iterator = %Array.prototype.values%` (and nothing else) per WebIDL —
Swiper spreads `[...el.attributes]`. `Element.part` is a `DOMTokenList`
whose setter hand-implements `PutForwards=value` (Swiper assigns
`el.part = "container"`). `HTMLElement.dir` reflects (Swiper reads
`el.dir.toLowerCase()`).

## v1 limitations

- No `event.target` retargeting per scope (bubbling crosses the boundary,
  listeners above it observe the inner target). Invisible to static
  screenshots.
- No declarative Shadow DOM (`<template shadowrootmode>`).
- No `slotchange` events (`assignedNodes()`/`assignedElements()` are
  implemented; `options.flatten` is ignored). `Text.assignedSlot` is absent
  (only `Element.assignedSlot`).
- No `:host(...)`/`:host-context()` function forms; `@scope` in shadow
  resolves to no implicit scope.
- `cloneNode` never clones shadow roots (spec-adjacent: only `clonable`
  roots clone, and `attachShadow` ignores `clonable`).
- `exportparts` is not forwarded (`imported_part` → `None`,
  `exports_any_part` → `false`).
- Constructed stylesheets expose no `cssRules` view (`NotSupportedError`);
  `replace`/`replaceSync` cover the observed usage.
- Incremental relayout disabled while shadow roots exist (full rebuild).

## Consequences

The target SPA's blocker is gone end-to-end: `attachShadow` no longer throws,
Swiper components construct, and a manually initialized `swiper-container`
builds its full shadow markup (styles, slots, parts). The remaining page
breakage is unrelated to Shadow DOM (missing `AbortController`,
`IntersectionObserver`, `ResizeObserver`, `performance.timing` keep the
Angular app from initializing `init="false"` swipers) — that is the next
feature block.
