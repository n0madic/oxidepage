# ADR-0019: Form controls, element state, focus, and FormData

- Status: accepted
- Date: 2026-07-14

## Context

Running jQuery 3.7.1 against the engine surfaced one coherent hole. Everything
else in jQuery worked — selectors, traversal, manipulation, `.data()`, events and
delegation, `$.Deferred`, `$.ajax`, and the whole effects pipeline — but `.val()`,
`.prop("checked")`, `.serialize()` and the `:checked` selector all failed, and they
failed for the same reason.

Re-running the same suite against **jQuery 2.2.4 and 4.0.0** then found a second,
sharper one, which §7 below covers: jQuery 4's ajax prefilter runs
`s.data instanceof window.FormData` on *every* `$.ajax()` call, so a missing
`FormData` global is not a missing convenience — it is a `TypeError` ("invalid
'instanceof' right operand") that takes down **all of jQuery 4's ajax**.

The form-control interfaces existed only as **empty stubs**:

```webidl
interface HTMLInputElement : HTMLElement {};
interface HTMLSelectElement : HTMLElement {};
```

They were introduced (ADR-0009 follow-up) so that `document.createElement("input")
instanceof HTMLInputElement` would hold and feature detection would not lie. But
no member was ever implemented, so `input.value`, `input.checked`,
`select.selectedIndex`, `option.selected` and `form.elements` were all `undefined`.
Worse than absent: assigning `input.value = "x"` silently created an **expando** on
the JS wrapper, so the write appeared to succeed and then had no effect on the DOM.

Underneath, `StyloElementState::element_state` existed and was already wired into
stylo (it is the return of `TElement::state`, and `build_snapshot` already recorded
it for invalidation) but **nothing ever wrote to it**. It was permanently empty, so
`:checked`, `:disabled`, `:enabled`, `:required` and friends matched nothing.

And `crates/dom/src/select.rs` hardcoded six pseudo-classes with a `_ => false`
fallback, so even a correctly populated state bit could not have matched.

## Decision

### 1. Form state is a distinct thing from content attributes

A control's `value` is not its `value` attribute. HTML gives each control a **dirty
value flag**: once script (or a user) writes `input.value`, the IDL attribute stops
tracking the content attribute, which keeps reading back as the *default*. The same
split governs `checked`/`defaultChecked` and `selected`/`defaultSelected`.

`crates/dom/src/form.rs` introduces `FormState`, held as `Option<Box<FormState>>` on
`ElementData` so that the elements which cannot have it pay one null pointer:

```rust
pub struct FormState {
    value: Option<String>,       // None = dirty value flag unset
    checkedness: Option<bool>,   // None = dirty checkedness flag unset
    indeterminate: bool,
}
```

Modelling each dirty flag **as** the `Option` is the whole trick: `None` means "not
dirty, fall back to the content attribute", so the two states cannot drift apart.

### 2. `update_element_state` is the single writer of `element_state`

It derives the stylo bits — `CHECKED`, `DISABLED`/`ENABLED`, `REQUIRED`/`OPTIONAL_`,
`READONLY`/`READWRITE`, `DEFAULT`, `INDETERMINATE`, `PLACEHOLDER_SHOWN`, `FOCUS`,
`FOCUS_WITHIN` — from the attributes plus the form state, and runs on every mutation
that can move one: the relevant attributes, the dirty flags, focus, and tree moves
(a `<fieldset disabled>` can gain descendants).

**It funnels into `note_subtree_mutation`, the one invalidation entry point.** An
earlier draft snapshotted the element and hinted a restyle directly, which looked
right and was not: `note_subtree_mutation` is also what bumps `style_version`, and
without that bump the next style flush early-outs and the restyle never runs at all.
A `input:checked + label` rule then never re-matched. The regression test
`page/tests/forms.rs::checking_a_box_from_script_restyles_a_checked_rule` pins this.

### 3. Pseudo-class matching defers to stylo's own mapping

`match_non_ts_pseudo_class` no longer keeps a hand-written list. Every pseudo-class
stylo supports other than the link ones is a pure element-state bit, and stylo
already publishes the mapping as `NonTSPseudoClass::state_flag()`:

```rust
other => {
    let flag = other.state_flag();
    !flag.is_empty() && self.element().stylo.element_state.contains(flag)
}
```

A bit we never set simply never matches — the honest answer — and one we later start
setting starts matching without a second edit here. `:link`/`:any-link` stay
structural (visitedness is deliberately untracked), and `:visited` is always false.

### 4. Selectedness invariants live in the DOM, not the bindings

The DOM owns the radio-group exclusivity rule and HTML's "ask for a reset" for
`<select>`, because both are invariants over *several* elements that no single IDL
setter can maintain. The reset has two clauses and both are load-bearing: options
arrive one at a time, so parsing `<select><option>x<option selected>y` auto-selects
`x` under the "nothing is selected" clause the moment it is the only option, and `y`
then arrives already selected. Without the counterpart clause ("if two or more are
selected, keep only the last") the select would finish the parse with **both**
selected and `:checked` would match twice.

### 5. Interaction: `click()`, `focus()`, `blur()`, `activeElement`

Headless, nobody clicks; script calling `el.click()` *is* the activation. `click()`
fires a cancellable `click`, then runs the element's **activation behavior** unless a
listener cancelled it. The only activation behaviors observable here are the form
controls': a checkbox toggles, a radio becomes checked, and both then fire `input`
and `change`.

> **Corrected by ADR-0020.** The order stated above is wrong: HTML's
> *legacy-pre-activation behavior* toggles the checkbox **before** the `click` event
> propagates, and a cancelled click *undoes* the toggle. Firing first and toggling
> after produces the same end state, which is why the tests here missed it — but it
> breaks React, whose `onChange` for a checkbox reads `node.checked` from inside the
> `click` listener.

Focus is a single `Option<NodeId>` on the tree. Moving it fires `blur`/`focusout`
then `focus`/`focusin` (the non-bubbling pair and the bubbling pair — jQuery
delegates focus through `focusin`, so both halves matter). Only a **connected**
element can hold focus, and `remove_internal` drops focus when the focused element
leaves the tree, so `activeElement` can never name a detached (or freed) node.

### 6. `long` in the codegen

`select.selectedIndex` is a WebIDL `long` and must round-trip `-1`. Signed `long`
*returns* already worked (they map to `RetKind::Number`); arguments were a build-time
error. `ArgKind::I32` + `BindCx::arg_i32` (ECMAScript ToInt32) closes that.

### 7. `FormData`, and one definition of "extract a body"

`FormData` is a real interface, not a marker object for the `instanceof` check that
demanded it. Constructing one from a `<form>` runs HTML's "construct the entry list"
over the form's *successful* controls — non-disabled, named, and (for a checkbox or
radio) checked — which is a projection of the form state above, not a re-derivation.
Values are strings: `Blob`/`File` do not exist here, so a file entry has nothing to
hold and `<input type=file>` contributes nothing, as it would with an empty selection.

A `FormData` that could be built but not *sent* would be the fake that P6 forbids, and
`fetch()` and `XHR.send()` each stringified their body independently — so a FormData
would have reached the wire as the string `"[object FormData]"` through whichever one
was not taught about it. They now share `imp::body::extract`, which returns the bytes
**and the body's default `Content-Type`**. That pairing is the point: a multipart body
is only coherent if the header names the same boundary that delimits the parts, and
the boundary is generated inside the extractor. The default loses to a `Content-Type`
the caller set — which is exactly why jQuery passes `contentType: false` for a
FormData body, and why obeying that is what makes jQuery's own ajax work.

The boundary carries 128 random bits. A guessable one would let a hostile *value*
close a part early and forge the remainder of the body.

## Consequences

**jQuery 2.2.4, 3.7.1 and 4.0.0 all run.** Across an 86-check suite covering every
jQuery module, the only failure on each is `$.parseXML`, which needs an XML parser the
engine does not have (a pre-existing, documented deviation — ADR-0017). The other
differences between the three are jQuery's own: 2.x has no `.catch` on a Deferred and
rewrites `%20`→`+` inside `param()` (3.0 moved that into `ajax()`), and 4.x dropped
`$.now`. The engine behaves identically under all three.

**15 WPT subtests flipped to PASS** without being targeted: checkbox/radio activation
behavior and its `input`/`change` events (`Event-dispatch-click`,
`Event-dispatch-detached-input-and-change`), dynamic `disabled`
(`event-disabled-dynamic`), label default action, and
`MutationObserver-attributes :: HTMLInputElement.type`.

**Deliberately absent (P6 — absent beats fake):**

- **`form.submit()`**. Submitting is a navigation, and the engine never navigates
  from script. A no-op `submit()` would make feature detection lie, so the member is
  not installed. `form.reset()` needs no navigation and *is* implemented.
- **Anchor activation behavior.** `a.click()` fires the event but does not follow the
  href. This has a visible cost: `url/javascript-urls.window.js` clicks an
  `<a href="javascript:…">` and awaits the navigation that would execute it, so it now
  waits out the harness budget where it previously failed fast at the missing
  `click()`. It is skipped, with that reason recorded, rather than having its TIMEOUT
  baked into the baseline. (Fixing the skip revealed that `SKIP_SUBSTRINGS` was being
  ignored for `url/` files, which is now repaired.)
- **Constraint validation.** No `checkValidity()`, no `validity`, and `:valid` /
  `:invalid` match nothing (their bits stay unset) rather than guessing.

**Other v1 limits:** `form.elements` is a plain `HTMLCollection`, not an
`HTMLFormControlsCollection` (which only adds a `namedItem` overload returning a
`RadioNodeList`); `FormData` holds strings only (no `Blob`/`File`, so no
`input.files`/`FileList`); no selection API (`setSelectionRange`); `input.type`
reflects the full keyword list but the engine renders every type as a plain box (there
is no widget layer), so `:checked` styles a box that never had a checkmark to begin
with.
