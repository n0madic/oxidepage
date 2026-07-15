# ADR-0020: Click activation order, and template contents

- Status: accepted
- Date: 2026-07-14

## Context

ADR-0019 closed the form-control gap that jQuery exposed. Running the *rendering*
frameworks — Vue 3.5.39 and 2.7.16, React 19.2.7 and 18.3.1, Preact 10, Alpine 3,
Lit 3 — is a much sharper test: jQuery mostly *queries* the DOM, while these
libraries build it, patch it, and synthesise their own event systems on top of it.

They found two defects. Both had been invisible to our own tests, and both are
instructive about *why* they were invisible.

### 1. The click activation behavior ran in the wrong order

ADR-0019 §5 described `click()` as "fire a cancellable `click`, then run the
element's activation behavior unless a listener cancelled it", and that is what
`imp::interaction::click` did: it computed the new checkedness, dispatched the
event, and only then wrote it.

That is backwards. DOM §2.9 runs the activation target's **legacy-pre-activation
behavior** *before* the event propagates, so a `click` listener already observes
the toggled checkbox; only the `input`/`change` events and, on cancellation, the
**legacy-canceled-activation behavior** come after.

Our tests could not see the difference, because the *end state* is identical
either way — the box ends up checked, and `input`/`change` still fire. We had even
written a test (`page/tests/forms.rs`) that asserted `click:false,input:true,
change:true` and called it correct, freezing the bug into the baseline.

React sees the difference immediately. It does not use the `change` event for a
checkbox or radio at all; it synthesises `onChange` from the native **`click`**,
and decides whether anything changed by comparing `node.checked` against a value
it recorded at mount (its "value tracker", which shadows the `value`/`checked`
accessor on the DOM node). Reading the *pre-toggle* value made that comparison
equal, so React concluded nothing had changed and **`onChange` never fired for any
checkbox or radio**. A `<select>` kept working throughout, because React takes the
raw `change` event for that one — which is exactly the asymmetry that pointed at
the click path.

### 2. `template.content` was never exposed

The parser has always put a `<template>`'s children into a separate contents
fragment rather than its child list; `DomTree` has carried `template_contents`,
cloned it, and serialised it since Phase 2. The one thing missing was the IDL
attribute, so from script `template.content` was `undefined` and the fragment was
unreachable.

`<template>` is not a corner of the platform. Alpine's `x-if` and `x-for` are
`<template>` elements; **lit-html compiles every template literal into a real
`<template>` and clones its `.content` on every render**; so does every hand-rolled
web component. And WPT was hiding the gap rather than reporting it: two test files
(`Event-dispatch-single-activation-behavior.html`, `pointer-event-document-move.html`)
read `template.content` in their *setup*, threw, registered no subtests, and were
recorded as a whole-file `__harness__ TIMEOUT` — one line in `expectations.tsv`
standing in for 132 subtests nobody was running.

## Decision

### Activation is pre-activation, dispatch, then activation-or-undo

`DomTree::legacy_pre_activation` applies the checkedness change and reports what it
changed; `DomTree::legacy_canceled_activation` puts it back. `imp::interaction::click`
now runs the first, dispatches, and then either fires `input`/`change` or undoes.

Both live in the DOM, not the bindings, for the reason ADR-0019 §4 already gives:
they are invariants over *several* elements. Undoing a radio click is not
"uncheck the one that was clicked" — it is "give the check back to the group member
that held it", and an empty group is a state the user's click never asked for. Only
the tree can know which member that was, and `set_checkedness` already owns the
group-exclusivity rule.

A checkbox's pre-activation also clears `indeterminate`, and the cancel restores it.

An already-checked radio activates nothing (`legacy_pre_activation` returns `None`),
which is what keeps it from firing `input`/`change` — as browsers do.

**Activation stays on the `click()` path rather than moving into `dispatch_event`.**
That looks like a shortcut and is not: DOM §2.9 sets the activation target only when
"event is a **MouseEvent** object and event's type attribute is `click`". We have no
`MouseEvent` (see below), so the only event in the engine that can activate anything
is the one `click()` fires — and putting the behavior there is exactly equivalent.
The day `MouseEvent` lands, the hook moves to `dispatch_event` and the predicate
becomes the spec's.

### `template.content` returns the contents fragment

`HTMLTemplateElement.content` hands over the fragment the DOM already had, creating
it on demand for a template that came from `document.createElement` rather than the
parser. `[SameObject]` holds for free: the wrapper cache is keyed by arena index, so
one fragment node always yields one JS object.

## Consequences

**The frameworks run.** Vue 3.5.39 45/45 and Vue 2.7.16 37/37, React 19.2.7 44/44
and 18.3.1 44/44, Preact 10 13/13, Alpine 3 10/10, lit-html 4/4 — across suites
covering reactivity, keyed list patching (asserting nodes are *moved*, not
recreated), synthetic and native event delegation, capture/bubble/`stopPropagation`,
controlled and uncontrolled form controls, portals/teleport, SVG namespacing, shadow
DOM, and layout geometry after a state-driven style change.

**WPT: +42 subtests PASS** (15762 → 15804). Three come from the activation fix
(`Event-dispatch-click.html`); the rest are `<template>` consumers — including 33 in
`Event-dispatch-single-activation-behavior.html`, a file that had never run a single
subtest. Its other 99 subtests now fail *honestly*, against activation behaviors we
deliberately do not implement (form submission, `<a>`/`<area>` navigation, `<details>`
toggling, `<label>`→control forwarding), and are recorded as such. Trading one
`TIMEOUT` line for 99 `FAIL` lines is the point of the expectation file: it is a
record of what is true, not a suppression list.

**Known gaps this work characterised but did not close** (all three leave feature
detection honest — P6):

- **`MouseEvent` is absent.** No library under test *constructs* one (React only
  reads `clientX`/`button`/modifier keys off a native event, and gets `undefined`),
  so nothing here is blocked. It costs the test tooling built on
  `dispatchEvent(new MouseEvent("click"))` — React Testing Library's `fireEvent`,
  Vue Test Utils' `trigger` — and it is what keeps activation off `dispatch_event`.
- **`MessageChannel` is absent.** React's *scheduler* falls back to `setTimeout`
  silently and correctly; only `act()` (a test-only API) hard-requires it.
- ~~**`[CEReactions]` timing** remains as ADR-0009 D4 recorded it: reactions are
  deferred to the microtask checkpoint, so `el.innerHTML = "<my-el>"` followed by a
  synchronous `querySelector("my-el").method()` sees an un-upgraded element
  (`document.createElement` upgrades synchronously, as the spec's synchronous flag
  requires). Everything still upgrades one turn later, which is why LitElement works
  — it is promise-based (`updateComplete`) and never assumes the synchronous
  timing. Overturning D4 means implementing the custom element reactions stack, and
  belongs in its own ADR.~~ **Closed by ADR-0021**: reactions now run synchronously
  through a custom element reactions stack, and `innerHTML = "<my-el>"` followed by a
  synchronous method call sees the upgraded element.
