# ADR-0017: Real `Document` nodes, `DOMImplementation`, and node ownership

- Status: accepted
- Date: 2026-07-12
- Supersedes: ADR-0012 **D4** ("Inert documents are shims over the main document")

## Context

Three `dom/nodes` WPT files produced **zero** subtests. They load
`tests/wpt/vendor/dom/common.js`, whose line 59 is `new Document()`. There was no
`Document` constructor, the exception was unhandled, and the harness ERRORed
before a single subtest ran — ~3500 subtests hidden behind three baseline lines.
The constructor was only the first blocker; `common.js` also needs
`createCDATASection`, a real `createDocumentType`, and `createProcessingInstruction`
on a second document.

ADR-0012 D4 was the only decision standing in the way: it made `DOMParser` and
`createHTMLDocument` return a JS shim object over the *live* document, on the
grounds that "the engine has a single live document, so a second real `Document`
was out of scope". The baseline design document is silent on multiple documents.

Re-reading the engine showed that premise was weaker than it looked. The
single-document assumption is not load-bearing anywhere structural:

- `NodeData::Document(DocumentData)` was already a per-node payload.
- The "document is arena slot `(0, gen 1)`" invariant holds because the document
  is allocated **first** into a fresh arena, not because it is unique.
- `propagate_connectedness` grants `IS_CONNECTED` only under `self.document`, and
  every style/layout/resource hook is gated on that flag — so a second document
  is inert *for free*.
- Hierarchy validity already rejected a Document as a child by **kind**, so a
  second document is structurally an eternal detached root.

## Decision

**D1 — A second `Document` is a real node, and inertness is structural, not
defensive.** `DomTree::create_document` allocates a Document that never gets
`IS_CONNECTED`. Style, layout, resource loading, custom-element upgrades, the
`getElementById` index, and event bubbling to the `Window` are all already gated
on that flag, so none of them see it. The one hook that fired regardless of
placement — `note_style_element_closed`, the parser's `<style>`-popped hook — is
now gated too; without that, a `<style>` in a `DOMParser` document would have
queued a `StyleUpdate` against the *page's* style engine.

**D2 — `Node.owner: Option<NodeId>` is the node document; `None` iff the node
*is* a Document.** The biconditional (rather than "`None` means the page
document") makes a missing owner a bug instead of a silent default. `NodeId`'s
generation is a `NonZeroU32`, so `Option<NodeId>` is niche-packed and the field
costs 8 bytes, not 12.

Ownership is maintained inside `insert_internal` — the single mutation path —
which implements the spec's "adopt node into parent's node document" step. The
parser sink, `graft_subtree_children`, and cloning inherit adoption for free,
and there is exactly one place to audit. An early-out on `owner == target` keeps
the common insert O(1), and is *exact* rather than heuristic because subtree
owners are uniform by induction.

**D3 — A pinned node pins its node document.** This is the one genuinely new
invariant, and the only place the existing machinery did not already suffice. A
node created by `doc2.createElement()` and never inserted is its **own** detached
root: it is not in doc2's subtree, so `subtree_has_pins(doc2)` cannot see it.
Without an owner pin, GC of the doc2 wrapper would free doc2 while that element is
still live, and `el.ownerDocument` would name a freed slot — which the spec makes
impossible.

So `pin`/`unpin` also increment/decrement the owner's count, and adoption *moves*
the pin. `pins[doc]` therefore counts the document's own wrappers **plus** one per
pinned node it owns; freeing only ever asks whether that total is zero, which is
exactly "is this document still referenced". Folding it into the existing `pins`
map means every existing free-refusal check works unchanged.

**D4 — Spec `isConnected` is decoupled from `NodeFlags::IS_CONNECTED`, and the
decoupling is surgical.** Per DOM, a node inside a `new Document()` *is*
connected (its shadow-including root is a Document). But the engine flag also
means "in the rendered document" and gates seven consumers. Widening it would
turn a second document into a live one.

So `DomTree::is_spec_connected` walks composed parents to the root and asks
whether it is a Document, and **only** the JS `Node.isConnected` getter uses it.
In particular `events.rs`'s path construction keeps the engine flag: it decides
whether the event path reaches the `Window`, and a `createHTMLDocument` document
has none.

**D5 — `CDATASection` is a Text node for every rule.** `interface CDATASection :
Text`, so hierarchy validity ("a Document must not have a Text child"), `:empty`,
whitespace classification, and layout all mean *it too*. Those tests route
through `Node::is_text` / `is_text_kind` rather than naming `NodeKind::Text`, so
a `match` cannot quietly forget the CDATA arm — the compiler demands it and would
happily have accepted the wrong (permissive) answer. `normalize()` is the
deliberate exception: it merges *exclusive* Text nodes, which a CDATASection is
not.

**D6 — `DOMParser` and `createHTMLDocument` are native, and the JS shim is
deleted.** `Sink` gained a `document` field (html5ever asks for the document
handle exactly once, at `TreeBuilder::new`), so `parse_into_document` runs the
real full-document parse into a second Document. Head-level content now lands in
`<head>` instead of being dropped or foster-parented. `createHTMLDocument` is
built from the spec's steps directly rather than by parsing a string.

**D7 — `DOMImplementation` carries its document.** It is not node-backed: it is a
slab object holding the `NodeId` it was minted for, so a saved `implementation`
keeps creating documents against *its* document — which
`DOMImplementation-createHTMLDocument-with-saved-implementation.html` checks
precisely.

**D8 — MutationObserver delivery becomes a real microtask.** Surfaced by this
work rather than caused by it: `MutationObserver-textContent.html` had three
subtests failing and the file timing out, and unblocking the CDATASection
subtest would have added a fourth. `microtask_checkpoint` drained the *whole*
promise-job queue and only then delivered records, so an `await
Promise.resolve()` overtook records queued before it — the inverse of the spec,
which enqueues the compound microtask when the first record is queued.

The compound microtask now rides the engine's promise-job queue (a pristine
`Promise.prototype.then` captured in `bootstrap.js`, so page script cannot
intercept it), enqueued from the host-call trampoline `cx::native` — the one
point where the mutating call has released its `dom` borrow and no further JS
has run. The spec's "mutation observer microtask queued" flag lives in
`PageState`. The checkpoint's trailing delivery stays as the fallback for
records queued outside JS (the parser). The file now passes 4/4.

Bundled here rather than split out because it is the direct cause of the only
non-PASS this change would otherwise have introduced, and because
`replace_all_with_text` — the other half of that file's failures, which queued
one record *per removed child* instead of one for the whole "replace all" — is
on this change's diff already.

**D9 — The arena carries a generation high-water mark across navigation.** Found
while surveying this work; a standalone correctness bug. Navigation replaced the
whole arena, and a fresh arena re-issued `(k, FIRST_GENERATION)` — so ids the old
document handed to script did **not** go stale, they *aliased* unrelated nodes of
the new document. `CLAUDE.md` documented a guarantee that did not exist.
`DomTree::with_generation_base` seeds the replacement arena above the outgoing
one's high-water mark. Slot 0 is the deliberate exception (`window.document` is a
non-configurable data property whose wrapper outlives the realm-wide navigation,
so its payload must keep resolving — to the incoming document).

## Consequences

**ADR-0012 D4 is superseded.** Inert documents are real Documents. Its
`bindings.rs:3129` regression test — which asserts *behaviour*, not the shim —
passes **unedited**, which is the evidence the Angular sanitizer/icon-registry
path did not regress.

**ADR-0009 is now stale on one point.** It declined `adoptedCallback` because
there was a single document. There are now several, and cross-document adoption
is real, so the reason no longer holds — but the callback is still not
implemented. Per P6 it stays absent rather than becoming a silent no-op; it is a
separate decision.

### Accepted deviations, named rather than discovered later

- **There is no XML parser.** `text/xml`, `application/xml`,
  `application/xhtml+xml` and `image/svg+xml` are parsed with the **HTML** parser
  into a document flagged with the requested content type. A strict superset of
  what the shim did, and it keeps inline-SVG parsing working — but it is not XML
  parsing, and a well-formedness error will not produce a `<parsererror>`.
- **`document.fonts` on a second document** is a per-document `[SameObject]`
  wrapper over the page's font state: identity holds, contents are the page's.
  Font loading is a property of a browsing context this document does not have.
- **`document.styleSheets` on a second document is empty**, honestly: its sheets
  are never registered with any stylist.
- **`adoptedStyleSheets` on a second document is stored but never applied.** This
  one was sharp: `sync_adopted_sheets` mapped "not a shadow root" to
  `scope = None`, which is the *page's document scope* — so
  `doc2.adoptedStyleSheets = [sheet]` would have restyled the page. It now
  returns early for any document that is not the page's.
- **`document.write` on an XML document throws `InvalidStateError`**; on a second
  HTML document it warns and no-ops, as it already did off the parser path.
  `open()`/`close()` are not implemented.
- **`qualifiedName` is `DOMString?`, not `[LegacyNullToEmptyString] DOMString`.**
  The generator has no type-level extended-attribute path, and adding one to
  silently coerce `null` → `""` would be a fake. `None` and `Some("")` both mean
  "no document element", which is what the spec and WPT require.

### On the WPT numbers

Unblocking three files that produced *zero* subtests adds ~3650 to the
denominator, so a dip in PASS% was expected and would not have been a
regression. It did not happen: **11414/16254 → 15591/19908**, i.e. +4177
absolute passes and 70.2% → 78.3%. `expectations.tsv` loses 529 lines and gains
6, all six of which are `__harness__ OK` — the healthy state 703 other files
already record. No previously-passing subtest acquired a FAIL line.
