# ADR-0003: Phase 2 JS-bindings implementation decisions

- Status: accepted
- Date: 2026-07-03

## Context

Phase 2 (design doc §10) wires JavaScript to the DOM: the `JsEngine`
abstraction with a QuickJS-NG backend, WebIDL codegen, the wrapper
identity/pin contract (§5.3), core DOM interfaces, `querySelector*`, event
loop v1, and console. Implementing it surfaced decisions the design document
left open or that experience contradicted. This ADR records them.

## Decision

1. **Scoped engine access instead of free realm methods.** rquickjs's
   runtime lock is not reentrant, so all engine operations live on an
   object-safe `JsScope` trait; host callbacks receive the *active* scope
   rather than the realm handle. `JsRealm::with_scope` is the only entry
   point. The design doc's `JsRealm` methods (`eval`, `register_class`, …)
   moved onto the scope.

2. **Class/prototype layout is bindings-level, not engine-level.** The
   engine crate exposes one native host-object class carrying an opaque
   `(tag, data)` payload plus GC-finalization reporting
   (`JsRealm::take_finalized`). Prototype chains, brand checks, and
   `HostClassDef`-style registration live in `oxidepage-bindings`, keeping
   the engine trait minimal and backend-portable.

3. **Weak wrapper cache in JS, pins in the DOM.** The one-wrapper-per-node
   cache is a JS-side `Map<slot-index, WeakRef>` (created by a bootstrap
   script), so the engine's GC drives wrapper lifetime; a class finalizer
   reports `(tag, data)` back to Rust, which unpins the node and frees
   fully-unpinned detached trees. Freeing is suppressed while the parser
   holds tree handles and while mutation observers have undelivered records
   (records hold `NodeId`s that pins do not cover).

4. **Event dispatch is implemented in `bindings`, not `dom`.** Listeners
   call back into JS and routinely mutate the DOM, so the tree must not stay
   mutably borrowed across a callback; dispatch runs as a driver loop with
   short borrows. The `dom` crate's native dispatch skeleton remains for
   engine-internal use. Listener identity/dedup uses JS `===` on callbacks.

5. **Indexed collections are Proxy-wrapped host objects.** `NodeList`,
   `HTMLCollection`, and `DOMTokenList` get WebIDL indexed/named property
   semantics from a JS `Proxy` created at wrap time; `host_payload` unwraps
   proxy chains so brand checks accept either the proxy or its target.
   Liveness comes from recomputing items on access — no invalidation
   protocol.

6. **Microtask ordering approximation.** `queueMicrotask` rides the engine
   promise-job queue (exact ordering with promise reactions);
   `MutationObserver` delivery runs at checkpoint boundaries after pending
   engine jobs, which can order observer callbacks after promise reactions
   queued later. Accepted for Phase 2; revisit if WPT failures point at it.

7. **One `HTMLElement` for all HTML elements.** Per-tag interfaces
   (including `HTMLUnknownElement` discrimination) are deferred; every
   HTML-namespace element wraps as `HTMLElement`, foreign elements as
   `Element`. The IDL and codegen already support adding subclasses.

8. **Codegen is checked in.** `cargo xtask codegen` regenerates
   `crates/bindings/src/generated.rs` from `crates/idl/webidl/*.webidl`
   (weedle2), formats it with rustfmt, and `--check` gates freshness in CI.
   Unknown IDL constructs are hard errors (scope-creep guard, §11).

9. **Known leak classes (bounded, per-page).** Cross-heap cycles
   (node ↔ listener closure) reclaim at teardown as designed; additionally,
   `MutationObserver` wrappers/callbacks and `[SameObject]` collection
   wrappers are held strongly for the page's lifetime, and detached
   subtrees that were never wrapped are freed only opportunistically
   (GC finalization of any wrapper in the tree) or at teardown.

## Consequences

- A V8 backend implements `JsEngine`/`JsRealm`/`JsScope` plus one host class
  with payload + finalization; nothing in `bindings` changes.
- Dispatch logic exists once in `bindings`; the `dom` skeleton is unused by
  the JS path (a small duplication accepted to keep borrows sound).
- The Proxy layer makes collection property access slower than native
  exotic objects; acceptable at Phase 2 document sizes, replaceable by
  engine-level exotic classes later without IDL changes.
- WPT expectations track the deliberate gaps (adoption/multi-document,
  per-tag element classes, `attributes`/`NamedNodeMap`, legacy aliases).
