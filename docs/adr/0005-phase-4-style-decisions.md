# ADR-0005: Phase 4 style-engine (stylo) implementation decisions

- Status: accepted
- Date: 2026-07-04

## Context

Phase 4 (design doc §10) integrates Servo's style engine, **stylo**, to give
the document a real cascade: the document stylesheet set (`<style>`,
`<link rel=stylesheet>`, `@import`), media queries, incremental restyle driven
from the single DOM mutation path, `getComputedStyle` (computed values), and
CSSOM. Wiring a mature, Gecko/Servo-shared engine into the arena DOM forced a
set of decisions the design document left open (and, in one case, corrects it).
This ADR records them.

## Decision

1. **stylo `=0.19.0` from crates.io, `servo` feature.** The version is pinned
   to exactly match the crates already in the tree — `selectors` 0.39,
   `cssparser` 0.37, `web_atoms` 0.2.5, `servo_arc` 0.4.3, `markup5ever` 0.39 —
   so **no transitive dependency is bumped**. Building stylo requires `python3`
   on `PATH` (it runs a mako code generator in `build.rs`); this is now a
   documented build prerequisite.

   Originally `=0.18.0`. The bump to `0.19.0` (with `selectors` 0.38 → 0.39)
   was forced by the media-feature table: the non-Gecko build of 0.18 compiles
   only `width`, `scan`, `resolution`, `device-pixel-ratio`,
   `-moz-device-pixel-ratio`, and `prefers-color-scheme`, so `@media
   (min-height: …)` and `(orientation: …)` parsed as invalid and never matched,
   in stylesheets and in `matchMedia` alike. 0.19 adds `height`, `orientation`,
   `aspect-ratio`, `device-width`/`device-height`, and the `pointer`/`hover`
   families. Its `Device::new` also takes an explicit device size and the
   pointer capabilities; a headless page reports the viewport scaled by DPR and
   `PointerCapabilities::empty()`.

2. **`dom` depends on stylo directly, and `OxideSelectorImpl` is deleted.** The
   engine's hand-rolled `SelectorImpl` is replaced by
   `style::selector_parser::SelectorImpl`, and a single element handle
   (`NodeRef`) backs **both** `querySelector` and stylo's cascade — the two can
   never disagree about what matches. This buries the "small DOM-only build"
   idea from design §4.2: the `dom` crate now unavoidably pulls in stylo.

3. **The element handle is pointer-sized, backed by a thread-local — this
   corrects the plan.** The plan specified `NodeRef = (&DomTree, NodeId)` (two
   words). That is impossible: stylo's style-sharing cache is a *fixed-size*
   thread-local buffer sized for a single-pointer element handle
   (`sharing::FakeCandidate._element: usize`), and a 16-byte handle fails the
   size assertion in `SharingCacheBase::new` at runtime. `blitz-dom` avoids
   this because its `BlitzNode = &Node` is one pointer. We match that
   constraint without a self-referential `Node`: `NodeRef` stores only the
   `NodeId` (8 bytes) plus `PhantomData<&DomTree>`, and recovers the tree from a
   thread-local (`ACTIVE_TREE`) installed by `select::enter_active_tree(tree)`
   for the duration of a query or a style traversal (an RAII `ActiveTreeGuard`).
   `querySelector`, `StyleEngine::resolve_styles`, and computed-value reads all
   run inside that scope. This is the single most load-bearing deviation from
   the plan.

4. **Per-element style state uses `UnsafeCell`/`Cell`/`AtomicBool`, not
   `AtomicRefCell`.** stylo 0.18 mutates element data through **shared**
   references during its traversal (it guarantees exclusive per-node access via
   its own threading model, which the borrow checker cannot see). We copy
   `blitz-dom`'s pattern: the cascade result lives in
   `UnsafeCell<Option<ElementDataWrapper>>` and the engine-facing flags
   (`selector_flags`, `dirty_descendants`, `has_snapshot`, `snapshot_handled`)
   are `Cell`/`AtomicBool`. `element_state` stays a plain field (only the `&mut`
   mutation path writes it). The design doc's `AtomicRefCell` sketch is stale.

5. **`SharedRwLock` + `SnapshotMap` live on `DomTree`; the engine holds a clone
   of the lock.** `TDocument::shared_lock()` returns the tree's lock, and
   snapshots for invalidation are taken and cleared by the tree along its single
   mutation path. `SharedRwLock::clone` shares the underlying lock, so guards
   from the engine's clone read data the tree locked (and vice versa).

6. **`bindings` depends on `oxidepage-style`; `style` does not depend on
   `bindings`.** `PageState` owns `Rc<RefCell<StyleEngine>>`, so CSSOM
   implementations call the engine directly rather than routing 20 methods
   through `HostHooks`.

7. **CSS byte decoding uses stylo's `Stylesheet::from_bytes`** (BOM >
   `Content-Type` charset > `@charset` > environment default), the spec
   algorithm, for external `<link>` stylesheets.

8. **`@import` uses a synchronous blocking loader.** stylo's `@import`
   machinery is asynchronous (the loader returns a *pending* rule the embedder
   fills in later). We sidestep it: `BlockingImportLoader` (a `StylesheetLoader`
   over a `CssFetcher` backed by `net.fetch_blocking`) fetches and parses
   `@import`ed sheets inline while the parent parses, guarded against cycles and
   capped at depth 8; refused/failed imports become `ImportSheet::Refused`.

9. **The user-agent stylesheet is our own** (~120 lines, MIT): display types,
   `head { display: none }`, `[hidden]`, and default margins, informed by the
   HTML rendering spec. `blitz-dom`'s `default.css` is MPL-2.0, which we avoid.

10. **`unsafe_code = deny` is scoped-allowed** only in `dom/src/stylo.rs`,
    `dom/src/stylo_data.rs`, the `NodeRef::tree()` thread-local deref in
    `dom/src/select.rs`, `style/src/properties.rs` (two index→enum transmutes,
    the same trick stylo uses), and the `RecalcStyle` `DomTraversal` impl in
    `style/src/engine.rs` (stylo's `ensure_data`/`unset_dirty_descendants` are
    `unsafe fn`). Each site carries a `SAFETY` comment citing stylo's exclusive
    per-node access contract.

## Incremental restyle

Every DOM mutation funnels through `note_children_changed` /
`note_subtree_mutation`, which insert a conservative
`RestyleHint::restyle_subtree()` on the nearest inclusive-ancestor element that
already has cascade data, and `mark_dirty_ancestors`, which propagates stylo's
`dirty_descendants` bit to the root. That propagation is **independent** of the
engine's own `NodeFlags::HAS_DIRTY_DESCENDANT` gate — the two have different
lifecycles (the engine flag is cleared by layout in Phase 5; stylo's bit is
cleared at the end of every style pass), and conflating them stops the restyle
from reaching the root. Attribute/state changes snapshot the element *before*
mutation for stylo's selector-invalidation; `set_viewport` calls
`force_stylesheet_origins_dirty` with the origins `set_device` reports as
media-changed, so viewport changes re-cascade.

## Traversal is sequential

`style::driver::traverse_dom(&traverser, token, None)` — no rayon pool. The
style-sharing cache and bloom filter are thread-locals; a single-threaded
traversal keeps the whole engine `!Send`-friendly and matches the page's
single-threaded event loop.

## v1 limitations (deferred to later phases)

- Shorthands serialize to `""` in the computed declaration (WPT `css/cssom` is
  overwhelmingly longhands).
- `used`/`resolved` values in `getComputedStyle` wait for layout (Phase 5);
  Phase 4 returns computed values.
- `:hover`/`:focus`/`:active` **parse** (stylo's grammar accepts them) but match
  nothing — `ElementState` is empty until interactivity exists (P6). This is a
  behavior change: those selectors were a `SyntaxError` in Phase 2.
- Presentational hints (`width=`, `bgcolor=`), quirks mode (always
  `NoQuirks`), Shadow DOM, container queries, animations/transitions
  (`DocumentAnimationSet` is empty), and `@font-face` loading are not
  implemented.
- No real font metrics: a `NoopFontMetricsProvider` returns empty metrics and a
  fixed 16px (13px monospace) base size, which is enough for the cascade.

## CSSOM bindings (WP-G/H/I)

The JS-facing CSSOM is generated from `crates/idl/webidl/cssom.webidl` like every
other interface, with a few deliberate departures:

11. **`CSSStyleDeclaration` is a live view, driven through a `styleProxy`.** The
    host object stores only what identifies its source — an element (for
    `el.style` and `getComputedStyle`) or a rule's locked block — and reads the
    declarations on every access, matching the DOM collections' "recompute, don't
    invalidate" model. `el.style` writes reserialize the block back to the
    `style` attribute, so the normal mutation path (snapshot + restyle) applies
    for free; rule writes mutate the locked block and call
    `note_style_rule_declarations_changed`. The open-ended camelCase/dashed
    property set (`style.backgroundColor`, `style["background-color"]`) and
    indexed access are handled by a `styleProxy` bootstrap wrapper seeded once
    with a name→property map, rather than hundreds of generated accessors — the
    same trade-off as the `collectionProxy`.

12. **The declaration-block, rule, and sheet operations live in
    `oxidepage_style::cssom`.** The bindings crate never names stylo's property
    or rule internals (decision 6): it calls plain-Rust helpers that take
    `&SharedRwLock` + handles and return `String`/`bool`/`Vec<String>`.

13. **Codegen grew integer attribute defaults** (`insertRule(rule, index = 0)`)
    and a `this_unwrap`/passthrough entry per CSSOM interface. `getComputedStyle`
    and the `Element.style` accessor (a `PutForwards=cssText` string setter) are
    hand-installed in `install_window` because the codegen cannot express them.

### CSSOM v1 limitations

- `@media`/`@import`/`@supports`/`@font-face` **parse and cascade**, but their
  CSSOM wrappers surface as the base `CSSRule` (correct `type`/`cssText`); the
  `CSSMediaRule`/`CSSImportRule`/`MediaList` interfaces and grouping-rule nesting
  (`CSSMediaRule.insertRule`) are not yet exposed — "absent beats fake".
- Wrapper identity is preserved without holding stale `Arc`s: a sheet/rule/list
  view stores its **owner node** and resolves the current stylesheet from the
  engine on each access, so a re-parsed `<style>` (a new underlying `Arc`) is
  followed rather than snapshotted. `document.styleSheets[i]` and `.cssRules`
  are `[SameObject]` (keyed by owner node, generation-qualified); `cssRules[i]`
  and `CSSStyleRule.style` are cached per list/rule wrapper so their identity
  holds while the rule set is stable.
- `getComputedStyle` performs a style flush of pending inline `<style>` updates
  so it reflects sheets added earlier in the same script; `<link>` loads stay
  queued for the event loop. On this synchronous path an inline `<style>`'s
  `@import` is not fetched (no blocking network in `getComputedStyle`), so an
  `@import` in a `<style>` added and read within one script tick is unresolved.
- Constructed sheets (`new CSSStyleSheet()`, `adoptedStyleSheets`) and
  `replace`/`replaceSync` are absent.

## Consequences

- The `dom` crate's dependency graph now includes stylo and its transitive
  tree; the first build is long, and CI needs `python3`.
- `NodeRef` handles must never escape an `enter_active_tree` scope; the
  `tree()` deref debug-asserts an active scope.
- The style engine is `!Send` (thread-local caches, `Rc`-shared lock), matching
  the page's single-threaded model.
