# ADR-0023: The UI event family and trusted input synthesis

- Status: accepted
- Date: 2026-07-27
- Builds on: ADR-0022 (navigation), whose recorded limit — "activation is wired
  to `click()` and nothing else, because the spec's activation trigger is a
  `MouseEvent` and that interface does not exist yet" — this ADR closes.

## Context

Stage 1 gave the page somewhere to go. Without this stage it could not be
*driven* once it got there: there was no `UIEvent`, `MouseEvent`,
`KeyboardEvent`, `PointerEvent`, `WheelEvent`, `FocusEvent` or `InputEvent` in
the IDL, and no path from a coordinate to a dispatched event. `click`, `type`,
`press` and `hover` are the next four verbs of every automation script, and
CDP's `Input` domain (a later stage) has nothing to call without them.

## Decision

### The typed payload lives in one boxed, optional field

`EventData` carried exactly one extra slot — `detail: JsValue` — shared by
`CustomEvent`, `PopStateEvent` and `SubmitEvent` on the reasoning that no event
is more than one of them. That does not scale to ~30 typed members across seven
interfaces, and widening `EventData` inline would put mouse coordinates in the
allocation of every `DOMContentLoaded`.

So `EventData` grew **one** field:

```rust
pub ui: Option<Box<UiPayload>>,
```

Every non-UI event pays one null pointer and no allocation. `UiPayload` holds
`detail` (`UIEvent`'s, a *different* member from `EventData::detail`), the
`view` flag, the modifier set, and a `UiKind` enum carrying the per-interface
fields.

**Getters brand-check on the payload shape, not the interface name.** A
`MouseEvent` getter on a `KeyboardEvent` receiver is a `TypeError` because the
`UiKind` match fails, not because of a per-interface slab tag. `WheelEvent` and
`PointerEvent` therefore share `UiKind::Mouse` — they *are* mouse events, and
`wheelEvent.clientX` has to work.

`relatedTarget` is stored as a `NodeId`, not a wrapper, so the generation check
stays at the read (`opt_node_to_js`) where every other node-valued member puts
it.

### Activation moved into `dispatch_event`

Previously `HTMLElement.click()` owned the activation protocol. A synthesized
mouse click would have needed the same protocol, and a second copy is how
`<label>`, submit and hyperlink activation drift apart.

Activation now runs **inside dispatch**, triggered by the DOM's actual
condition: an event of type `click` carrying a mouse payload. `click()`,
`dispatchEvent(new MouseEvent("click"))` and a synthesized pointer click all
reach it through one path. A plain `Event` named `"click"` deliberately still
does not activate — the spec's trigger is the interface, not the type.

Three consequences the WPT run forced, each a real spec rule the old code had
collapsed:

- **The `disabled` check is per element type.** HTML's input activation
  behavior returns early only when the element is *neither* a checkbox nor a
  radio, so a **disabled checkbox still toggles** on a dispatched click and
  `preventDefault()` still undoes it. Only the non-checkable behaviors are
  suppressed by `disabled`.
- **It is re-checked after the dispatch.** The activation target is chosen
  before the event propagates, so a listener can disable it in between.
- **The ancestor walk is gated on `bubbles`.** DOM sets the activation target
  from an ancestor only "if event's bubbles is true"; a non-bubbling `click` at
  a text node inside a checkbox activates nothing.

### Event handler IDL attributes participate in the bubble phase

Found while landing the above, and much larger than the stage that surfaced it:
`onclick` and friends were invoked **only in the target phase**. Delegation —
one `onclick` on a container handling clicks on its descendants — did not work
at all, nor did `onclick="…"` content attributes on any ancestor.

An event handler *is* an event listener (HTML registers it, non-capturing, when
first set), so it participates at the target and while bubbling, and never
while capturing. Fixed.

**Known deviation, deliberate:** HTML puts the handler at the position in the
listener list where it was *first assigned*, so an `onclick` set before an
`addEventListener("click")` should run first. Here the handler always runs last
on its target. Matching the spec would mean registering handlers as real
listeners; ordering between the two on one element is not something real code
depends on.

### `:hover` and `:active`

`DomTree::set_hovered`/`set_active` mirror `set_focused` exactly, because the
problem is the same: the state belongs to whole **inclusive-ancestor chains**
(`:hover` matches every ancestor of the hovered element), so both the old and
new chains are re-derived through `update_element_state`. No `select.rs` change
was needed — `match_non_ts_pseudo_class` already defers generically to stylo's
`state_flag()`, and `stylo_dom` has `HOVER`/`ACTIVE`.

Removing a hovered or active node clears the state, alongside the existing
focus cleanup — a stale `NodeId` left in those slots would be walked later.

### `pointer-events` is honoured by hit testing

Hit testing ignored the property entirely. An element with
`pointer-events: none` is now transparent to the test and the point falls
through to what is behind it. Descendants are *not* excluded: the property is
inherited, so a child setting `pointer-events: auto` back is hit normally.
Without this, every page with a full-viewport overlay is undriveable.

### Synthesis produces sequences, not events

`imp::input_synth` is the engine side; the interface `imp` modules stay
data-only. A mouse input produces what a browser produces:

- **Move** diffs the previous hover chain against the new one:
  `mouseout`/`mouseover` (bubbling, carrying `relatedTarget`) plus
  `mouseleave`/`mouseenter`, which do **not** bubble and are fired individually
  on each element actually left or entered — leave innermost-first, enter
  outermost-first.
- **Down** fires `pointerdown` then `mousedown`, sets `:active`, and moves focus
  to the nearest focusable inclusive ancestor **only if the event was not
  cancelled**. `preventDefault()` on `mousedown` suppressing focus is how every
  dropdown and combobox library keeps focus in a custom control.
- **Up** fires `pointerup`, `mouseup`, then `click` at the nearest common
  inclusive ancestor of the press and release targets.

The `click` is a `PointerEvent` (HTML's "fire a synthetic pointer event"), which
is what makes it reach the activation behavior above.

Coordinates are viewport CSS pixels throughout — what `elements_from_point`
takes and what `clientX/Y` mean. `pageX/Y` add the document scroll at *read*
time (a listener may have scrolled since dispatch); `offsetX/Y` subtract the
target's padding-box origin, resolved at construction.

Every step re-validates its node ids. A listener between two events of one
sequence can remove the element under the pointer or navigate, and a stale
`NodeId` panics in `Arena::node`.

`Page::dispatch_mouse` is **not** a new task source: it is an embedder call on
the page thread, the same position `Page::eval` occupies. A listener that
navigates queues on `pending_navigation` and the following `run_until_stalled`
performs it — Stage 1's existing contract.

### `javascript:` URLs

ADR-0022 left these warn-and-skip. Now that activation actually reaches a link,
that gap became two hangs, so HTML's "navigate to a `javascript:` URL" is
implemented: the percent-decoded payload runs as a classic script, and **only a
string result replaces the document**. `javascript:void 0` and every
`javascript:doThing()` handler return `undefined` and must leave the page
exactly as it was. Queued as a `PendingNavigation`, not run inline, for the same
reason every other navigation is.

### Focus events are real `FocusEvent`s

`blur`/`focus`/`focusout`/`focusin` now carry `relatedTarget` — the element on
the other side of the transfer. A focus manager reads it to decide whether focus
left its subtree at all.

## Consequences

WPT went from 16129 to 16246 passing subtests; 123 tracked non-PASS entries were
removed and 6 added.

**The 6 additions are a knowingly accepted divergence**, and the reasoning is
recorded here rather than buried in a rebaseline. They are all of
`Event-dispatch-single-activation-behavior.html`'s **nested-form** combinations:
a `<form>` made a DOM descendant of another `<form>`, which the HTML parser
cannot produce — the test builds it through `appendChild`.

They regressed because the ancestor form's `onsubmit` now runs when the inner
form's `submit` event bubbles through it. Checked against Chrome 150 rather than
assumed:

| | Chrome |
| --- | --- |
| plain bubbling event through an `<input>` | propagates normally |
| nested form's `submit` → ancestor form, **capture** | fires |
| nested form's `submit` → ancestor form, **bubble** | **does not fire** |
| same `submit` → `document`, bubble | fires |

Chrome suppresses the bubble-phase listeners on an ancestor *form* specifically,
while propagation continues past it to the document. That rule is not derivable
from the DOM or HTML specs, and reproducing it would mean guessing. Reverting
the handler fix instead would restore a bug that breaks `onclick` delegation on
the entire real web, for six subtests in a DOM shape HTML forbids. The trade is
recorded, not hidden; if the mechanism is later identified, this is the entry to
revisit.

## Deliberate non-goals (P6 — absent beats fake)

Touch and gesture events; `Selection`/`Range` over arbitrary DOM;
`contenteditable`; IME/composition *generation* (the `CompositionEvent`
interface exists and is constructible — `Event-subclasses-constructors.html`
checks it — but nothing fires one, because there is no IME in a headless
process); drag-and-drop; `DataTransfer`; clipboard; pointer coalescing and
prediction; `:focus-visible`.

`:hover` is verified with `getComputedStyle` after a synthesized move, **not**
as a reftest: `xtask/src/reftest.rs` renders a file with `load_html` + `settle`
and has no way to synthesize input. Extending the runner for one test is not
worth it; the assertion is equally strong.
