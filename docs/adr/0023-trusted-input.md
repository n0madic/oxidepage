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

`relatedTarget` is stored as the node's **wrapper**, the same choice
`SubmitEvent.submitter` makes (ADR-0022 §9). A `NodeId` was tried first, on the
theory that the generation check could stay at the read — but a wrapper *pins*
its node and a bare id does not, so a detached related target the GC collected
left the payload naming a freed slot. That is not a null at the getter: dispatch
hands the related target to the retargeting walk below, which reads it through
`DomTree::containing_shadow_root`/`node` and **panics** on a stale id, unwinding
out of a JS host call.

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
target's padding-box origin, resolved at construction. **Both sides of that
subtraction are viewport coordinates** — `LayoutEngine::padding_box` resolves
through `absolute_origin(.., include_scroll: true)` — so the document scroll
appears in neither. Adding it to one of them offset every `offsetX/Y` by exactly
the scroll position, invisibly on any test that never scrolls first.

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

### Typing, and why `change` waits for blur

`dispatch_key` fires `keydown` → `keypress` (printable keys only — deprecated,
and still what jQuery and every hotkey library listen for) → the default action
→ `keyup`. The default action is where editing happens: `beforeinput`
(cancelable) → mutate → `input`. A `keydown` that a listener cancels suppresses
the edit and nothing else.

**`change` is not fired there.** A text control fires it on **blur**, and only
when the value differs from the one it had when focus *arrived*. That cannot be
recomputed after the fact, so `move_focus` snapshots the value as focus lands
and compares on the way out. Getting this wrong is invisible to a test that
checks only the final value, and breaks every form that validates on `change`.

`maxlength` caps **user** edits only — assigning `value` from script bypasses
it — so the check lives in the synthesis path, not in the DOM layer.

Enter's default action is **implicit submission**, and HTML scopes that to the
*input* text states (`DomTree::allows_implicit_submission`), not to every text
entry control. A `<textarea>` is a text entry control and Enter inserts a
newline there; submitting its form instead both navigated away and lost the line
break, since the `"Enter"` arm also shadows the printable-text arm.

Selection offsets are UTF-16 code units, the units script compares against
`value.length`. `selectionStart`/`selectionEnd` are typed `any` in the IDL
rather than `unsigned long?`, because the generator has no nullable-number
return and the distinction is load-bearing: a control without text entry must
report **null**, since 0 is a valid caret position and feature detection reads
the null.

`sequential_focus_order` implements HTML's rule — positive `tabindex` first in
ascending order, ties by document order, then everything with `tabindex="0"` or
a native default. `tabindex="-1"` is focusable but absent from the order, which
is exactly what the negative value means. Tab wraps, because a single browsing
context has no chrome to hand focus back to.

### `scrollIntoView`, and un-skipping its corpus

`Element.scrollIntoView(arg)` is implemented over the existing `scroll_parent`
chain, walking **every** scrollable ancestor innermost-first — an element nested
in a scroll container inside the document has to end up visible in both, and
scrolling only the nearest one leaves it off-screen, which is the first thing an
automation driver hits. Each step re-reads the element's rect, because the
previous scroll moved it.

The position handed to the alignment is the element's **visual delta from the
container's near edge** — `border_box.origin - padding_box(container).origin`,
both already viewport-relative — and the container's current scroll offset is
added exactly once, by the alignment itself. Adding it to the delta as well
counted it twice: a second `scrollIntoView()` (a no-op in a browser) scrolled by
the whole distance again, and `Align::Nearest`'s "the element is above the
visible top" branch became unreachable, since a doubled delta is never negative.
The viewport path never had the bug, which is what made the two disagree.

`behavior: "smooth"` is treated as **instant**, and that is a documented limit:
there is no animation timeline here, and a driver wants the final position.

ADR-0006 §8 skipped the whole vendored `scrollIntoView` corpus as "out of
scope". That reason is gone, so the skips are gone: 38 files now run, and
`mouseEvent.html` with them. Two are skipped **by name**, with the reason
recorded, because they can only ever hang rather than fail:
`scrollIntoView-container.html` (asserts propagation to outer frames) and
`mouseEvent-offsetXY-svg.html` (drives the pointer through WPT's `test_driver`
protocol, which the runner does not implement).

Everything else is left running and its failures are **tracked**, not
suppressed — a FAIL belongs in the expectations file, which is the repo's
standing contract. The bulk of them (~90) are the vertical and sideways
writing-mode files: the engine lays out `horizontal-tb` only, an orthogonal
pre-existing limit that has nothing to do with `scrollIntoView`. Core behavior
passes — `scrollIntoView-horizontal-tb-writing-mode.html` 9/9, plus the shadow
DOM and partially-visible cases.

`MouseEvent.offsetX/offsetY` are stored as an `Option`: `None` for a
*constructed* event, which has no target and whose `offsetX` the spec makes
equal to `pageX` — and `pageX` tracks the document scroll at read time, so it
cannot be precomputed. `mouseEvent.html` pins exactly this.

### `relatedTarget` retargeting

DOM's **retarget** algorithm now runs on dispatch: while the related target is a
node whose root is a shadow root, and the dispatch target is not a
shadow-including descendant of that root, the related target becomes the root's
host. A listener in the light tree therefore sees the *host*, never the node the
pointer actually came from — which is the entire point of a closed tree.

"Clear targets" is decided **before** the dispatch, not after: a listener is
free to move the target out of its shadow tree mid-dispatch, and the decision
has to reflect where it was when the event started. It is applied before the
activation behavior, which the spec orders the same way.

The retargeting is not gated on "a shadow root is involved" as originally
planned — the guard turned out to be the retarget loop itself, which returns
immediately for a related target that is not in a shadow tree. That leaves the
common path a single `containing_shadow_root` call rather than a branch.

**Not implemented, and tracked as two WPT failures:** the early-return branch of
dispatch step 5 (when the target and the retargeted related target coincide) —
the spec text and the test's expectations did not reconcile under inspection,
and guessing at it is worse than a recorded gap.

### `XMLHttpRequest` is a real `EventTarget`

A third failure of that test needed `XMLHttpRequest.dispatchEvent`, which did
not exist: XHR ran an entirely parallel event system — its own
`Vec<(String, JsValue)>` of listeners, its own handler-property struct, and a
`make_event` that built a plain `{type, target}` object.

That stand-in was the real cost, well beyond the one subtest: on an XHR event
`preventDefault()`, `stopPropagation()`, `currentTarget`, `isTrusted`,
`composedPath()` and `instanceof Event` **all failed**, and the listener list
supported none of `capture`, `once`, `passive`, `===` deduplication or
`handleEvent` objects.

The fix was mostly deletion, because the infrastructure was already generic: the
listener registry and `event_handlers` map are keyed by `EventTargetKey`, and
`dispatch_event` already had a `Host` path. `XMLHttpRequest : EventTarget` in
the IDL, one `match` arm in `this_event_target`, and the slab key stored on
`XhrData` as its event-target identity — the same scheme `new EventTarget()`
uses. The `onX` properties moved into the shared registry, which is what puts
them on equal footing with `addEventListener` rather than in a separate list
invoked first.

Slab keys are monotonic and never recycled, so a freed XHR's key can never
alias a new one. Their registry entries *were* leaking, though — for
`new EventTarget()` too — so finalization now drops the listeners and handlers
under the key alongside the slab entry.

Still absent at the time of this ADR, and the next honest gap: `ProgressEvent`
(with `loaded`/`total`/`lengthComputable`), `xhr.upload`, and the
`progress`/`timeout` events. XHR fires plain `Event`s for now.

**Closed by ADR-0024**, which also fixed four state-machine bugs this stage left
untouched — chief among them that a *reused* XHR fired no events at all,
because the wrapper root released here on every terminal transition was never
restored by `open()`.

### Wheel

`Page::dispatch_wheel` fires a **cancelable** `wheel` and respects the answer: a
carousel or modal that calls `preventDefault()` to trap scrolling actually traps
it, and a driver that ignored the return would scroll the page out from under
such a widget. Otherwise the nearest scrollable **inclusive** ancestor scrolls —
walking outwards past any container that cannot move in that direction, which is
what makes a wheel over a bottomed-out inner panel scroll the page instead.

"Inclusive" is `LayoutEngine::is_scroll_container`, and it exists because
`scroll_parent` cannot answer this: it is `Element.scrollParent()` and starts at
the box's *parent* by definition. `elements_from_point` returns the container
itself whenever the point does not land on an element child of it — a scroller
whose content is text, or a point beside its only child — so routing the wheel
through `scroll_parent` alone scrolled the document instead of the container the
pointer was over.

`document.hasFocus()` answers `true` for the rendered document — one browsing
context, never backgrounded — and `false` for a document with none
(`DOMParser`, `createHTMLDocument`). Real code gates polling loops, autosave and
focus traps on it and would idle forever against a constant `false`.

### Focus events are real `FocusEvent`s

`blur`/`focus`/`focusout`/`focusin` now carry `relatedTarget` — the element on
the other side of the transfer. A focus manager reads it to decide whether focus
left its subtree at all.

## Consequences

WPT went from 16129 to 16303 passing subtests. 123 tracked non-PASS entries were
removed by the event and activation work and 6 added (below); un-skipping the
`scrollIntoView`/`mouseEvent` corpus then added 201 subtests to the run, of
which 57 pass.

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

Smooth scrolling stays out (`smooth-scroll` remains skipped: its async tests
wait on an animation that never runs).

`:hover` is verified with `getComputedStyle` after a synthesized move, **not**
as a reftest: `xtask/src/reftest.rs` renders a file with `load_html` + `settle`
and has no way to synthesize input. Extending the runner for one test is not
worth it; the assertion is equally strong.
