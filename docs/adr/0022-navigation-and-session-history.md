# ADR-0022: Navigation and session history

- Status: accepted
- Date: 2026-07-27
- Reverses: ADR-0019's two deliberate absences — `form.submit()` and anchor
  activation behavior — whose stated premise ("the engine never navigates from
  script") this ADR removes.

## Context

The engine could load a document and never leave one. Everything downstream of
that was shaped by the gap:

- `window.location` was a plain object of hand-installed **getters**
  (`install_location` in `crates/bindings/src/lib.rs`). No `href` setter, no
  `assign`/`replace`/`reload`.
- `window.history` was a **closure in `bootstrap.js`** over a JS array, with a
  native escape hatch — `__oxide_setDocumentUrl(url) -> bool` — whose only job
  was to move the document URL for a same-origin `pushState`. `go()` walked the
  array and synthesised a `popstate` by `defineProperty`-ing a `state` onto a
  plain `Event`; `go(0)` was a documented no-op.
- `a.click()` fired the event and stopped (ADR-0019).
- `form.submit()` was **not installed at all** — P6's "absent beats fake", on
  the reasoning that a no-op `submit()` would make feature detection lie.
- `document.referrer` was hardcoded `""`, with a comment explaining that this
  was always correct *because* script could not navigate a document into
  existence from another one.

The costs were already recorded rather than hidden. ADR-0019 skipped WPT's
`url/javascript-urls.window.js` (it clicks an `<a href="javascript:…">` and
awaits a navigation that could not happen, so the file timed out instead of
failing fast). ADR-0020 recorded 99 honest FAILs in
`Event-dispatch-single-activation-behavior.html` against activation behaviors
the engine did not implement — the first two named are form submission and
`<a>`/`<area>` navigation. And the automation roadmap puts this first for a
blunt reason: automation is "go somewhere, do something, go somewhere else", so
every "click through to the next page" flow was dead regardless of which
protocol sits on top.

## Decision

### 1. Navigation is a task source, not a call

A `location.href` setter runs inside the glue trampoline, under live `RefCell`
borrows on the DOM, style and layout engines. Committing a document **replaces
all three** — `reset_document_state` assigns a fresh `DomTree`, `StyleEngine`
and `LayoutEngine` into those same cells. Navigating inline is therefore not
slow or risky, it is a `BorrowMutError`; it is CLAUDE.md's "reflow must never
re-enter JS" seen from the other side.

So a script navigation *queues*. `PageState::pending_navigation`
(`crates/bindings/src/state.rs`) holds a `VecDeque<PendingNavigation>`, and the
page's event loop performs them — exactly like `pending_scroll_targets`.

- **Only a load collapses.** `location.href = a; location.href = b` in one task
  navigates once, to `b`: a queued `Load` supersedes a queued `Load`, which is
  the collapsing a browser does when several navigations are queued before the
  loop next spins. Nothing else collapses. A **traversal is cumulative** —
  `history.back(); history.back()` must move two entries, and a single
  last-write-wins slot turned that into one — and a `javascript:` URL is a
  script to run, not a destination to supersede. The queue is capped at
  `MAX_PENDING_NAVIGATIONS` = 32, since a script looping on `history.back()`
  would otherwise grow it without bound while the page performs at most
  `MAX_CHAINED_NAVIGATIONS` of them per chain anyway.
- **It is drained first.** `run_until_stalled_until` takes one immediately after
  `process_finalized` and then `continue`s the loop rather than finishing the
  pass (`crates/page/src/lib.rs`). Every drain step below it is keyed on
  nodes of the *outgoing* tree, whose ids the replacement arena deliberately
  makes stale. It checks the loop deadline **before** that `continue`: the chain
  counter below resets on every entry to this branch, so a page that leaves work
  queued after each chain re-enters it forever, and the deadline is the only
  backstop `Page::settle(budget)` has.
- **Two guards keep the drain out of its own way.** `navigating` is held for the
  whole of `load_document`, which runs the event loop internally (parser
  scripts, `await_subresources`, the trailing `run_until_stalled`) — without it
  the loop's own navigation drain would start a second load underneath the
  first. `parsing` keeps a parser-inserted script from pulling the tree out from
  under the parser holding handles into it. In both cases the request simply
  stays queued for the outer driver.
- **A navigation may chain.** `run_navigation` (`crates/page/src/lib.rs:1131`)
  loops while each completed navigation leaves another queued, capped at
  `MAX_CHAINED_NAVIGATIONS` = 20. `location.href = location.href` in a `load`
  handler is an infinite loop in a browser too; the difference is that a browser
  has a user who can close the tab and a headless engine has a caller waiting
  for a return. Exceeding the cap is a console error plus a `Failed` navigation
  event — not a panic, and not an `Err` that would make an unrelated
  `Page::navigate` look like it failed.
- **`reset_for_navigation` deliberately does not clear the queue**
  (`crates/bindings/src/state.rs:1143`). The commit path takes the request
  before it starts loading, so nothing stale is left for the incoming document —
  and a navigation queued by the outgoing document's last script must survive
  into the next turn of the loop rather than be dropped by the load it is
  chained off.

Failure is asymmetric, on purpose. An embedder's `Page::navigate` returns `Err`:
that caller asked for this URL and needs to hear that it did not load. A
*script*-initiated navigation that fails keeps the current document and logs the
reason — the page is not blanked, it simply did not move, which is what browsers
do.

### 2. `Page` is uniformly `&self`

The drain happens inside `run_until_stalled_until(&self)`, so navigation had to
be callable through a shared reference. `navigate`, `load_html`, `load_document`
and `reset_document_state` took `&mut self` for one reason only: two fields were
plain values rather than cells. `load_fired: bool` and `start_time: Instant`
became `Cell`s, and every `&mut self` on `Page` disappeared with them.

**This is a public API signature change.** `let mut page = Page::new(…)` becomes
`let page = Page::new(…)`. Every other method on `Page` was already `&self`, so
the type is now uniformly interior-mutable and an embedder can hold a `&Page`
across a navigation the loop performs.

### 3. Traversal is decided by `document_seq`, and there is no bfcache

`SessionHistory` (`crates/bindings/src/state.rs:198`) bumps a `document_seq` on
every cross-document commit and stamps it onto the entry that commit produces. A
traversal target is same-document **iff** `entry.document_seq` equals the current
`document_seq` — one integer comparison. Comparing URLs instead would get
`pushState` to the same URL wrong in both directions.

- **Same-document**: move the index, set the document URL, scroll to the
  fragment, fire `popstate` carrying that entry's state, then `hashchange` if the
  fragment actually changed. Both events come after the scroll, per HTML's "apply
  the history step".
- **Otherwise**: reload `entry.url` and `restamp` the entry with the freshly
  loaded document's sequence and the URL that load ended on (a redirect may have
  moved it), so a second traversal back to the same entry is same-document.
  `restamp` **preserves the entry's state** — that is what distinguishes it from
  `replace`.

There is no bfcache, so traversing out of the current document is always a
reload. That is correct, just slower than a browser — and observable: the
document is rebuilt, so the script state of the page being returned to is gone.
Implementing a bfcache means keeping a whole document, realm and wrapper set
alive per entry, which is a phase of its own, not a refinement of this one.

Two rules fall out of the same place. `history.go(0)` is `location.reload()` —
the one delta that is a real navigation even though the index does not move. And
the first cross-document commit *replaces* the initial `about:blank` entry
rather than pushing (`commit_history`, `crates/page/src/lib.rs:1380`), so a fresh
page that loads a document has one history entry, not two.

### 4. `Location` and `History` are real IDL interfaces

Both shims are deleted: the `bootstrap.js` `history` closure and the
`__oxide_setDocumentUrl` native hook that existed only to serve it.

A closure object is not an interface. `history instanceof History` was a
`ReferenceError`, `Object.prototype.toString.call(history)` answered
`[object Object]`, and every member was an own property rather than a prototype
one — the same shape ADR-0019 rejected for the form controls, for the same
reason: feature detection has to be able to see the truth.

`this` for each is a **brand token** in the slab (`HostData::Location`,
`HostData::History`). Neither interface holds state of its own: a Location *is*
the document URL, which lives in the DOM, and the entry list is
`PageState::history`. There is nothing to keep in sync because there is no second
copy. The wrapper itself is realm-stable (`location_js` / `history_js`, the
`navigator_js` pattern), so `location === location` survives a navigation — the
realm does.

`window.location = "/x"` is HTML's `[PutForwards=href]`: the window property is
an **accessor** whose setter calls `assign()`, not a rebinding of the property.

**Cross-origin writes are allowed on `Location`.** Navigating away from the
current origin is what a Location is *for*. The same-origin restriction belongs
where it always did — `pushState`/`replaceState`, which move the document URL
**without** loading, and which throw `SecurityError` for a cross-origin target.
That check compares the `(scheme, host, port)` tuple rather than
`Url::origin()`: non-special schemes (`file:`) get a fresh *opaque* origin per
parse, so `origin()` would fail every local test document against itself.

The codegen took three changes, each of which was a build-time error before it
was a feature — the drift protection working as designed: two `this_unwrap` arms;
a signed-default argument path (`optional long delta = 0` → `arg_i32_or`, with
the `ArgDefault::U32` literal checked to fit `i32`); and the nullable-string arm
widened to the optional form, since `optional USVString? = null` and `USVString?`
read identically (a missing argument is `undefined`, which maps to `None` exactly
as an explicit `null` does).

### 5. Activation behavior, still reached only through `HTMLElement.click()`

ADR-0020 kept activation on the `click()` path rather than moving it into
`dispatch_event`, because DOM §2.9 sets the activation target only when "event is
a **MouseEvent** object and event's type attribute is `click`", and `MouseEvent`
does not exist here. That reasoning is unchanged and now has teeth:
**`dispatchEvent(new Event("click"))` still does not navigate.** The day
`MouseEvent` lands, the hook moves and the predicate becomes the spec's.

Two nodes are in play and they are not the same one. The event is dispatched **at
the clicked node**; the behavior belongs to the **activation target**, the nearest
inclusive ancestor that has one (`activation_target`,
`crates/bindings/src/imp/interaction.rs:65`). Clicking a `<span>` inside an `<a>`
fires `click` at the span and follows the link.

`activation_target` walks **`flat_tree_parent`**, not `parent`. A click inside a
shadow tree resolves out through its host, which is the flat-tree invariant
CLAUDE.md states — applied here to a third consumer alongside stylo's restyle
traversal and box-tree construction. A disabled control has *no* activation
behavior rather than a suppressed one, so the walk continues **past** it to an
ancestor.

The behaviors are `Checkable` (ADR-0020's pre-activation, unchanged),
`Hyperlink`, `Submit { form }`, `Reset { form }` and `Label { control }`.

#### `<label>` is in the list even when it does nothing

The plan listed `<label>`→control forwarding as out of scope. Leaving it out did
not merely fail to add behavior — it **regressed six WPT subtests that had been
passing**, and the reason is the shape of the activation-target walk rather than
anything about labels.

**A no-op activation behavior is not the same as no activation behavior.** The
activation target is the *innermost* element that has a behavior at all, so an
element with none is transparent: the walk continues past it. Without a `<label>`
variant, a `<label>` stopped shadowing its ancestors, and clicking a
`<button type=button>` (which correctly has no behavior) nested in a `<label>`
nested in an `<input type=reset>` walked all the way out and **fired the reset**.
`Label { control: Option<NodeId> }` fixes that by existing: a label whose
`control` is `None` is reached, does nothing, and stops the walk.

The behavior itself is HTML's: run the *synthetic click activation steps* on the
labeled control (`DomTree::label_control`) — implemented by recursing through
`click()` rather than duplicating the pre-activation dance, so a label for a
checkbox goes through the same legacy-pre-activation path a direct click does.
`control` is `None`, and the label therefore inert, in two cases:

- the click started **on the labeled control itself**, which is what stops a
  click on the checkbox from toggling it twice; and
- the click started on **interactive content** inside the label (`<a href>`,
  `<button>`, `<select>`, `<textarea>`, a non-hidden `<input>`, …). That
  element's own activation — or its deliberate lack of one — is what the click
  means, so the label stays out of it.

### 6. An event handler that returns `false` cancels the event

This is not a navigation feature; it is a general event-dispatch correction that
navigation made visible.

The engine had always **discarded** an event-handler IDL attribute's return
value. HTML's *event handler processing algorithm* step 5 says a handler that
returns `false` cancels the event, and `onsubmit="…; return false"` /
`onclick="return false"` are the canonical cancel idioms — older than
`preventDefault()` and still everywhere. The bug was invisible for exactly as
long as the engine had no default actions worth cancelling. Once submission and
link activation existed, WPT's
`Event-dispatch-single-activation-behavior.html` — which uses the `onsubmit`
form throughout — **navigated away from itself**, and a file that had been a
tracked set of FAILs became a whole-file HARNESS TIMEOUT.

The fix is one arm at the end of `dispatch_event`'s handler branch
(`crates/bindings/src/events.rs`), calling `imp::event::set_canceled_flag`
(promoted to `pub(crate)` for it). Three scoping decisions are load-bearing:

- **Only an exact `false`.** `undefined` — what a handler that just runs
  statements returns, i.e. almost all of them — must not cancel anything.
- **The IDL-attribute path only.** A listener registered with
  `addEventListener` has no return value by design, which is why the loop above
  keeps discarding its result. The two paths are not symmetric and the spec does
  not make them so.
- **No special cases.** `onerror` on the Window inverts the test and
  `beforeunload` uses the value differently again; neither exists here, so
  neither is written down as a branch that could not be exercised.

### 7. Form submission is HTML's algorithm, ending in a queued navigation

`crates/bindings/src/imp/form_submit.rs` owns the whole thing and finishes by
queueing a `PendingNavigation` like every other navigation.

- **`submit()` versus `requestSubmit()`.** HTML distinguishes the two entry
  points by whether the `submit` event fires: `form.submit()` submits *without*
  firing it (and without validating), while `requestSubmit()` and a submit
  button's activation fire it and honour `preventDefault()`. One `fire_event`
  flag, not two algorithms.
- **A re-entrancy latch, `PageState::submitting`.** An `onsubmit` handler that
  calls `form.submit()` would otherwise recurse until the script budget kills the
  page. This is HTML's own "constructing entry list" flag.
- After the event returns, the form is re-checked for connectedness — a listener
  may have detached it.
- **Three encodings, all real.** A GET rewrites the action URL's query (and drops
  the action's fragment, per "mutate action URL"); a POST carries
  `application/x-www-form-urlencoded`, `multipart/form-data` — reusing ADR-0019's
  entry-list construction and its 128-bit random boundary — or `text/plain`. The
  `text/plain` serializer is five lines of its own rather than a quiet reuse of
  the urlencoded one: it is a distinct wire format, and encoding it as something
  else is exactly the fake P6 forbids.
- **`NetRequest::form_navigation`** (`crates/net/src/fetch.rs:158`) uses
  `RequestMode::Navigate`, which is what makes a cross-origin form POST work: it
  is exempt from the CORS checks a script `fetch` faces and keeps the
  author-chosen `Content-Type`. It also derives an `Origin` header from the
  referrer, as a browser does for a POST but not for a GET navigation.
- The submitter travels the whole way through the algorithm because
  `formaction`/`formmethod`/`formenctype` override the form's own — a single form
  with several destinations. Those attributes are now reflected on `<button>` and
  `<input>` from one shared module (`imp::form_support`), `formMethod` and
  `formEnctype` with the "limited to only known values" rule.

### 8. `document.referrer` is a real value

It is the URL of the document the navigation *left*, written by the page at
commit time. `""` when there is no predecessor — an embedder `Page::navigate`, or
the initial document — and `""` for an inert `DOMParser` /
`createHTMLDocument` document, which has no browsing context to have been
navigated within (ADR-0017).

`NetRequest::navigation_with` (`crates/net/src/fetch.rs:138`) takes the referrer
as a parameter rather than deriving it. At request-build time the engine still
has the outgoing document, but only the caller knows whether this navigation has
a predecessor at all.

### 9. `PopStateEvent` and `SubmitEvent` share the one extra event slot

`EventData` already carried a single extra `JsValue` for `CustomEvent.detail`.
`PopStateEvent.state` and `SubmitEvent.submitter` are each *the* one extra value
their interface carries, and no event is more than one of the three — so they
read the same slot under different names instead of growing the struct by two
permanently-null fields on every event the engine dispatches.

`SubmitEvent.submitter` holds the submitter's **wrapper**, not its `NodeId`. A
wrapper pins its node, so an event parked in a listener's closure cannot end up
naming a freed slot; the id is recovered, generation-checked, on read. A raw id
here would have been precisely the cross-task-boundary snapshot CLAUDE.md warns
about.

`MouseEvent.relatedTarget` and `FocusEvent.relatedTarget` now follow the same
rule, and it is not merely tidiness there: dispatch feeds the related target to
the shadow-DOM retargeting walk, which reads it through `DomTree::node` — a
freed detached related target was a **panic** out of a JS host call, not a null.

### 10. The `NavigationEvent` stream

`Page::drain_navigation_events` yields
`{ Started, SameDocument, Committed, DomContentLoaded, Load, NetworkIdle, Failed }`
with the URL the event is about, an optional error, and an epoch-millisecond
timestamp.

It is the engine's own record of what happened to this page, and it is shaped for
the consumer the roadmap names: a CDP layer renames `Committed` to
`Page.frameNavigated` and the rest to `Page.lifecycleEvent` without inventing
anything. `NetworkIdle` records a distinction `settle` already draws internally
but never surfaced — idle *reached* (no timer, no pending rAF, nothing in flight)
versus the budget running out. Building it now rather than with the protocol
keeps it testable from the library, where it is far cheaper to get right.

The stream is a milestone log **per navigation**, and bounded on both counts.
`NetworkIdle` is recorded once per document, not once per `settle`: every
`eval`/`dispatch_mouse`/`dispatch_key` ends in a settle that reaches idle, so
recording it each time made the stream grow with input rather than with
navigation. And nothing forces an embedder to drain it, so the vector is capped
at `MAX_NAVIGATION_EVENTS` = 1024 with the oldest dropped — the same reasoning
`MAX_HISTORY_ENTRIES` applies to the session history.

## Consequences

**Two deliberate absences from ADR-0019 are reversed, and their premise is
gone.** ADR-0019 recorded:

> **`form.submit()`**. Submitting is a navigation, and the engine never
> navigates from script. […] **Anchor activation behavior.** `a.click()` fires
> the event but does not follow the href.

Both were correct then and both rested on the same sentence. The engine now
navigates from script, so `form.submit()` and `requestSubmit()` are installed and
do the whole algorithm, and an `<a href>`/`<area href>` activation follows the
link. ADR-0020's note that the honest FAILs in
`Event-dispatch-single-activation-behavior.html` stand against unimplemented
activation behaviors now names three fewer: form submission, `<a>`/`<area>`
navigation and `<label>`→control forwarding are implemented, leaving `<details>`
toggling.

**The `javascript-urls` WPT skip stands, with a narrower reason.** It is no
longer "the engine never navigates from script" — it is that `javascript:` URLs
specifically are not implemented (see the limits below), so the awaited
navigation still never happens.

**Verification.** `crates/bindings/tests/bindings.rs::history_state_stack` pins
the interface shape and the two rules that are easy to get subtly wrong —
`history instanceof History`, the `[object History]` tag, that `history.state` is
a structured *clone* (mutating the object afterwards must not reach into the
entry), and that a cross-origin `pushState` throws `SecurityError` **and leaves
the document URL where it was**. `crates/page/tests/forms.rs` pins the submission
surface feature detection sees and `requestSubmit`'s argument validation
(`TypeError` for a non-submit button, `NotFoundError` for one owned by another
form). End-to-end behavior — a link click reaching a second document, GET and
POST submissions against a loopback server, `back()`/`forward()` across
documents, and the `NavigationEvent` sequence — belongs in
`crates/page/tests/navigation.rs`, which `forms.rs` already points at and which
must land with this change.

**WPT: 61 tracked non-PASS entries removed and 3 rewritten**
(`tests/wpt/expectations.tsv`), all in already-vendored directories and none of
them targeted:

- `dom/events/Event-dispatch-single-activation-behavior.html` — **55**. The file
  ADR-0020 brought back from a whole-file `TIMEOUT` and left failing honestly
  against activation behaviors that did not exist. Most of them now exist.
- `dom/events/Event-dispatch-click.html` — 2, and
  `dom/events/preventDefault-during-activation-behavior.html` — 1.
- `css/cssom-view/scroll-behavior-smooth-navigation.html` — 3 lines replaced
  rather than removed: `TIMEOUT`/`NOTRUN` became two `FAIL`s plus a harness `OK`.
  The file no longer hangs on a history navigation that could not happen, so it
  runs to completion and fails for a real reason (smooth scrolling). That is the
  expectation file doing its job — trading a hang for two honest failures is a
  gain, not a regression.

The §6 `return false` fix is what keeps the first of these from *costing* a
baseline: without it that file navigated away from itself mid-run and the whole
thing reverted to a HARNESS TIMEOUT.

WPT's `html/browsers/history/` and `html/semantics/forms/` are **not** added to
the runner's `RUN_DIRS`: neither directory is vendored under
`tests/wpt/vendor/`, and pulling them in needs network, which CI never has.
Vendoring them is a separate, mechanical change.

**Deliberate v1 limits.** Where an unimplemented behavior would otherwise be an
unexplained non-event, it warns on the console. That is not a softening of P6: an
API we do not implement is still *absent*, but these are APIs that are present
and whose effect is out of reach, so the page keeps running and the gap is
announced rather than swallowed (`BindCx::warn`).

- **`<input type=image>`** — not a listed control, so it has no coordinates to
  submit.
- **The `formdata` event.**
- **Constraint validation.** `novalidate` and `formNoValidate` reflect, but
  nothing validates; `checkValidity()`, `validity` and `:valid`/`:invalid` remain
  absent per ADR-0019.
- **`<details>`/`<summary>`** — no activation behavior, and no `open` toggling.
- **`javascript:` URLs**, as a link href or a form action: warns and skips.
- **`<meta http-equiv=refresh>`** — parsed by nobody; the charset form of
  `http-equiv` is handled by the byte decoder and is unrelated.
- **`beforeunload` / `unload`.** No unload event is dispatched, so a navigation
  is never cancellable and never observed by the outgoing document.
  `onbeforeunload` is a reflected event-handler attribute that predates this work
  and nothing will ever fire at it.
- **`target`.** There is one browsing context, so a `target` link navigates in
  place and warns. `download` suppresses the navigation and warns; `method=dialog`
  warns (there is no `<dialog>`).
- **No bfcache**, as §3 covers, and `history.scrollRestoration` is therefore
  stored and reflected only — there is no restorable scroll position behind it.
  Storing it is still the honest option: the value is observable.
- **`HashChangeEvent` is absent**, so `hashchange` is dispatched as a plain
  `Event` and `e.oldURL`/`e.newURL` are honestly `undefined` (P6) rather than
  values fabricated to fill an interface we did not implement.
- **Session history is capped at `MAX_HISTORY_ENTRIES` = 50**
  (`crates/bindings/src/state.rs:212`). This is not arbitrary frugality: an entry
  holds a live `JsValue` state across navigations, so an unbounded entry list is
  unbounded JS retention driven by page script. Older entries fall off the front
  and the index moves with them, which is what a browser's own cap does.
