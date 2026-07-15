# ADR-0021: Synchronous custom-element reactions

- Status: accepted
- Date: 2026-07-14
- Supersedes: ADR-0009 **D4** ("Reactions are delivered at the microtask checkpoint, not synchronously after each `[CEReactions]` method")

## Context

ADR-0009 D4 deferred every custom-element reaction (upgrade, `connectedCallback`,
`disconnectedCallback`, `attributeChangedCallback`) to the microtask checkpoint, with
two hand-picked synchronous exceptions (`document.createElement`, `customElements.upgrade`).
The justification was that the gap is "observably different from a browser only for
code that reads custom-element state within the same task right after a mutation —
not a pattern real frameworks depend on."

That justification was falsified. `container.innerHTML = '<my-el>';
container.querySelector('my-el').method()` — the ordinary way to instantiate a web
component from markup — called `method()` on an **un-upgraded** element, because the
upgrade reaction that `innerHTML =` enqueued only ran at the next checkpoint. The same
gap hit `customElements.define()` executed over already-parsed markup, `appendChild`
(whose `connectedCallback` ran one turn late), and `innerHTML =` itself. Nothing threw
and every element still upgraded eventually, so libraries built on it kept working —
Lit survives because its own API (`updateComplete`) is promise-based and never assumes
synchronous timing — but the timing itself was wrong, and wrong silently: nothing in
the engine reported a late upgrade as a gap the way P6 asks of a missing API.

The HTML spec does not defer these reactions. `[CEReactions]` marks operations and
attribute setters that must invoke their queued reactions **before returning to
script**, via the *custom element reactions stack*: each such call pushes a new
element queue, runs, and pops it — invoking every reaction the queue accumulated —
before it returns. Reactions raised with no such call on the stack (essentially, the
parser) fall to the *backup element queue*, drained at the next microtask checkpoint.
D4 only ever implemented the backup queue and generalized it to every reaction.

## Decision

### The element queue is a mark into the existing FIFO

The DOM already owned a flat FIFO, `DomTree::custom_reactions: Vec<CustomElementReaction>`
(ADR-0009 D2). Rather than allocate a real stack of per-call element queues, the stack
is modeled as marks into that one FIFO:

- "push a new element queue" = record `queue.len()` on entry, a plain `usize`
  (`DomTree::custom_reaction_mark`, `crates/dom/src/tree.rs:543`);
- "pop the element queue and invoke its reactions" = invoke every entry above the mark,
  FIFO, until the queue has drained back down to it
  (`DomTree::pop_custom_reaction_from`, `crates/dom/src/tree.rs:553`, driven by
  `invoke_custom_element_reactions`, `crates/bindings/src/lib.rs:1462`).

A nested `[CEReactions]` call takes its mark *above* the enclosing call's, so the two
slices can never interleave: the Rust call stack **is** the reactions stack, with no
allocation per call. Reactions enqueued with nothing on the stack sit below every mark
(mark `0`) and are exactly the spec's backup element queue — still drained at the
microtask checkpoint and at each event-loop tick
(`drain_custom_element_reactions`, `crates/bindings/src/lib.rs:1442`, now a thin
`invoke_custom_element_reactions(cx, 0)` wrapper).

### The scoping is IDL-driven, and a codegen gap is now closed

`[CEReactions]` was already present on 121 members across the `dom`/`html`/`cssom`
WebIDL — the codegen simply never read extended attributes, so it had no effect.
`crates/idl/src/lib.rs` now destructures `.attributes` on every attribute and
operation member — interface, partial-interface, and mixin members alike, so
`ParentNode`/`ChildNode` mixin methods carry the annotation into every interface that
includes them — and emits `define_method_ce` / `define_accessor_ce` instead of
`define_method` / `define_accessor` wherever `[CEReactions]` is present
(`ce_reactions`, `crates/idl/src/lib.rs:575`). That expands to 137 CE-scoped glue
registrations. Only the *setter* half of a `[CEReactions]` attribute is scoped — the
getter enqueues nothing, so it costs nothing.

Reading extended attributes at all exposed a latent gap in the "no silent API gaps"
posture (P6): an attribute the codegen did not recognize simply had no effect, so a
typo'd `[CEReaction]` would have compiled clean and silently done nothing. The codegen
now whitelists the attributes it knowingly ignores and has a concrete reason for each —
`SameObject`/`NewObject` (identity already handled by hand in `imp`), `Unscopable`
(no `with` support), `PutForwards` (hand-implemented setters), `LegacyNullToEmptyString`
(hand-written coercion) — and turns any other unrecognized extended attribute into a
hard `CodegenError` (`IGNORED_EXTENDED_ATTRS`, `crates/idl/src/lib.rs:564`).
`[CEReactions]` on a member kind whose glue cannot host a scope (a const, a
constructor, an event-handler attribute) is likewise a build-time error
(`reject_ce_reactions`, `crates/idl/src/lib.rs:601`), not a silently-ignored
annotation.

### `native_ce` in the trampoline

`crates/bindings/src/cx.rs` factors the single glue trampoline, `native`, over a `ce`
flag (`native_inner`, `crates/bindings/src/cx.rs:66`). The `[CEReactions]` variant,
`native_ce` (`crates/bindings/src/cx.rs:62`), takes the reaction-queue mark before
calling into the glue function and, only if the call succeeded, invokes
`invoke_custom_element_reactions(&cx, mark)` — placed after `sync_named_properties`
(so the reactions see up-to-date named-property access) and before
`queue_mutation_microtask` and `drain_pinned_connectivity`. Ordering the reactions
drain *before* the mutation-microtask enqueue means a `MutationRecord` a reaction
itself queues (a `connectedCallback` that mutates the DOM) joins the same compound
microtask as records from the outer call, preserving the ordering guarantee
`CLAUDE.md` describes for `MutationObserver` delivery.

Gating on `is_ok()` mirrors the sibling hooks on the same trampoline: a DOM operation
validates its arguments before it mutates, so a call that threw enqueued nothing.
Were that invariant ever violated, no reaction is lost — an entry left in the FIFO
below no active mark falls through to the enclosing operation's own drain, or, at top
level, to the microtask checkpoint's backup-queue drain.

### The registry operations that were always meant to be scoped

`customElements.define()` and `.upgrade()` carry `[CEReactions]` per spec
(`crates/idl/webidl/custom_elements.webidl`) — an annotation the IDL had never
carried before this change, since it had no effect prior to §2. `define()` now
upgrades already-parsed elements still on the page before it returns to the caller
that just called it, rather than one turn later. `upgrade()`
(`crates/bindings/src/imp/custom_element_registry.rs:190`) dropped its hand-rolled
`drain_custom_element_reactions(cx)` call: that call drained the *entire* FIFO,
including any backup-queue entries left over from the parser, which is more than the
spec's "invoke custom element reactions" for this call's own queue. Delivery is now
purely the trampoline's job, scoped correctly to the mark `native_ce` took on entry.

### A latent fragment-parse bug this change would have made visible

`create_element_in` (`crates/dom/src/tree.rs:1097`) enqueues an `Upgrade` intent for
any element created with `owner == document`. `parse_fragment_into` parses into a
**separate** `DomTree` — correctly, per spec's browsing-context-less temporary
document — and `graft_subtree_children` → `copy_node_from` then re-creates each node
in the real tree. Because that re-creation's `owner` is the real document,
`create_element_in` enqueued an upgrade even when grafting into a **detached** host
(`host.innerHTML = '<x-el>'`, where `host` is not in the rendered document). That was
already off-spec — a temp document has no registry, and creation there does not try
to upgrade; only the *insertion* steps try to upgrade, and only when the parent is
connected — but invisible while nothing observed the result before the next
checkpoint quietly upgraded it anyway. Once reactions drain inside the very
`innerHTML =` call that triggered them, that upgrade would have become observable
one line later, on an element the spec keeps `undefined`.

Fixed by splitting a `try_upgrade` parameter out of `create_element_in`
(`create_element_in_inner`, `crates/dom/src/tree.rs:1114`). `copy_node_from` now
calls it with `try_upgrade: false` — creation never upgrades a grafted node.
Insertion into a tree that *is* connected still upgrades it, through the existing
`note_custom_element_connect` path — the spec's actual rule. `graft_subtree_children`
is the only caller that passes `false`, so the fix's blast radius is exactly the
fragment-parse path; `cloneNode` goes through the unrelated `clone_node_inner` and
keeps enqueueing on creation, which is correct — `cloneNode` **is** `[CEReactions]`
and does upgrade its detached copy immediately.

`document.createElement` (`crates/bindings/src/imp/document.rs`) keeps its own
inline `upgrade_element` call before the glue builds the returned wrapper — it must
run the constructor before that wrapper exists. The `[CEReactions]` drain that now
also wraps `createElement` is a no-op there: `upgrade_element` only acts on an
`Undefined` element, and by the time the drain runs the inline call has already
turned it `Custom` or left it `Undefined` for good reason (not yet defined).

## Consequences

**The idiom works.** `container.innerHTML = '<my-el>';
container.querySelector('my-el').method()`, `appendChild` followed by a
synchronous read of `connectedCallback`'s effect, and `customElements.define()`
called after the markup already exists on the page, all now match browser timing.
`crates/page/tests/custom_elements.rs` asserts this **synchronously** — no
`settle()` — in `define_upgrades_parsed_elements_before_it_returns`,
`append_child_runs_connected_callback_before_it_returns`,
`inner_html_upgrades_before_the_setter_returns`, and
`a_reaction_that_mutates_the_dom_nests` (a reaction that itself mutates the DOM,
exercising the nested-mark case). `a_detached_fragment_is_not_upgraded` is the §6
regression test: it asserts the grafted element stays `Undefined` even after a
`settle()`, then upgrades once actually connected. Two existing tests were
rewritten to assert synchronously instead of after a `settle()`:
`remove_runs_disconnected_callback_before_it_returns` (previously
`disconnected_callback_fires_on_remove`) and `attribute_changed_only_for_observed`.

**Residuals, left as documented limits, not follow-ups:**

- **Re-entrancy.** A reaction runs JS from inside the trampoline; a
  `connectedCallback` that appends another instance of itself recurses until
  QuickJS's own stack limit or the script budget trips — the same hazard a real
  browser has, not a regression this change introduces.
- **One flat FIFO, not one queue per element** (ADR-0009 D2, unchanged). The spec's
  element queue is a list of *elements*, each carrying its own ordered reaction
  list; this engine keeps the single flat FIFO and only marks positions in it. The
  two models diverge exclusively when one operation enqueues two or more reactions
  for the same element interleaved with another element's — a corner deliberately
  left as a documented residual, not restructured here.
- **`adoptedCallback`** remains unimplemented (no `Adopted` reaction variant exists
  to schedule), and customized-built-in elements (`is=`), `ElementInternals`, and
  form-associated custom elements remain out of scope — all pre-existing ADR-0009
  limits, unaffected by this change.
