# ADR-0009: Autonomous custom elements

- Status: accepted; **D4 superseded by ADR-0021**
- Date: 2026-07-10

## Context

JS-heavy sites that register web components (Angular + Swiper, Lit, Stencil, …)
rendered broken because `window.customElements` did not exist: the first
`customElements.define(...)` threw a `TypeError`, the application's ES module
rejected, and the whole bootstrap failed — the server HTML stayed, but every
JS-driven layout adjustment (collapsing menus, initializing components) never
ran.

This ADR records the implementation of a spec-correct `CustomElementRegistry`
for **autonomous** custom elements (`class X extends HTMLElement {}`):
`define`/`get`/`getName`/`whenDefined`/`upgrade`, the lifecycle reactions
(constructor upgrade, `connectedCallback`, `disconnectedCallback`,
`attributeChangedCallback` filtered by `observedAttributes`), and their timing.

## Decisions

**D1 — Autonomous only; `HTMLElement` is the one constructible interface.**
Customized built-in elements (`is="…"`, `{ extends: … }`) are **not** supported
(as in Safari). Only `HTMLElement` gains a constructor; every per-tag interface
stays `Illegal`. A custom element's local name is its own tag (`<x-foo>`), never
an upgraded `<button is=…>`.

**D2 — Layered split: definitions in bindings, state + intents in the DOM.**
The `dom` crate cannot depend on JS, so constructors and lifecycle callbacks
live in `bindings::PageState::custom_elements` (a `CustomElementRegistry`
holding the JS values). The DOM stores only, per element, a
`CustomElementState` (`Undefined` / `Uncustomized` / `Custom` / `Failed`) and a
FIFO queue of **reaction intents** (`Upgrade` / `Connected` / `Disconnected` /
`AttributeChanged`) — pure data, no `JsValue`. `define()` tells the DOM which
names are defined; the DOM decides which elements get intents; bindings match an
intent back to a constructor/callback when the queue drains. This mirrors the
existing `image_updates` / `script_updates` intent queues.

**D3 — `HTMLElement` is a `Native` constructor bound through a construction
stack.** WebIDL `constructor()` makes the interface `Native` (codegen), so the
existing QuickJS subclass trampoline applies: it passes `new.target` as
`call.this` and pins the returned object's prototype to `new.target.prototype`.
Upgrade runs the author class via a bootstrap helper
`Reflect.construct(C, [], C)` so `new.target = C`. The base `HTMLElement`
constructor reverse-looks-up its definition by strict-equality on `new.target`,
then binds to the pre-created node via a single **construction stack** in the
registry (the node is pushed before the call and popped by the base
constructor). `new X()` with an empty stack instead creates a fresh,
disconnected, already-`Custom` element.

**D4 — Reactions are delivered at the microtask checkpoint, not synchronously
after each `[CEReactions]` method.** **Superseded by ADR-0021**: this deferral
turned out to be observable after all (`innerHTML = '<my-el>'` followed by a
synchronous method call on the new element saw it un-upgraded), and reactions
are now invoked synchronously by the `[CEReactions]`-scoped operation that
raised them, via a custom element reactions stack modeled as marks into the
same FIFO this decision describes. `microtask_checkpoint` drains the reaction
queue (before `MutationObserver` delivery, so reaction-driven mutations are
observable) and loops until both settle. Because every entry into JS ends with a
checkpoint, reactions fire at each script/task/microtask boundary. The one
**synchronous** exception is `document.createElement` of a defined name and
`customElements.upgrade(root)`, which the spec requires to upgrade immediately;
both run the constructor inline. This is observably different from a browser
only for code that reads custom-element state within the *same* task right after
a mutation — not a pattern real frameworks depend on.

**D5 — Upgraded wrappers are retained strongly.** QuickJS is
reference-counted and the generic node-wrapper cache is **weak**, so a wrapper
with no strong JS reference is freed the instant the last one drops. A custom
element's JS state (its subclass prototype and constructor-set instance fields)
lives *only* on that wrapper and cannot be rebuilt from the DOM, so
`PageState::custom_wrappers` holds a strong reference per upgraded element,
cleared on navigation. Without this, `const el = document.createElement('x-y')`
returned a fresh, un-upgraded wrapper because the upgraded one had already been
collected.

**D6 — Reset on navigation.** The realm survives navigation, so
`PageState::reset_for_navigation` clears the registry (definitions,
`whenDefined` promises, construction stack), the strong wrapper map, and the
DOM's defined-names mirror + reaction queue.

## Mechanics

- **Name validation** (`is_valid_custom_element_name`, in `dom`) implements the
  PotentialCustomElementName grammar and the reserved-name list.
- **`create_element`** (DOM choke point for `createElement` *and* the parser)
  sets `Undefined` for a valid custom name in the HTML namespace, and enqueues
  `Upgrade` if the name is already defined.
- **Connectedness** (`insert_internal` / `remove_internal`) enqueues
  `Connected` for `Custom` elements entering the tree, `Upgrade` for defined
  `Undefined` ones, and `Disconnected` (captured before detach) for `Custom`
  ones leaving.
- **Attribute mutations** enqueue `AttributeChanged` only for `Custom` elements;
  the `observedAttributes` filter is applied on the bindings side at drain time.
- **Upgrade delivery** runs the constructor, then synchronously delivers the
  initial `attributeChanged` for each present observed attribute followed by
  `connectedCallback` if connected (spec order: ctor → attributeChanged →
  connected).

## v1 limitations (not skips)

- **Customized built-in elements** unsupported (D1).
- **`adoptedCallback`** not implemented (single document; no cross-document
  adoption).
- **Form-associated custom elements / `ElementInternals`** unsupported.
- **Shadow DOM** and the `:defined` pseudo-class unsupported.
- **`connectedMoveCallback`** unsupported.
- **Single construction stack** rather than per-definition: correct for
  autonomous elements; pathological re-entrant constructors are out of scope.
- **Retained wrappers leak until navigation** (D5): a page that creates and
  discards many custom elements keeps their wrappers alive for the document's
  lifetime. Bounded by navigation; acceptable for the headless screenshot use
  case.
- **Reactions deferred to the microtask checkpoint** rather than the
  `[CEReactions]` backup-element-queue timing (D4), except for the synchronous
  `createElement` / `upgrade` paths. **Superseded by ADR-0021** — reactions are
  now invoked synchronously by the `[CEReactions]`-scoped call that raised
  them; only reactions raised outside any such call (the parser's) still use
  the backup element queue at the microtask checkpoint, which is the spec's
  own rule rather than a limitation.
