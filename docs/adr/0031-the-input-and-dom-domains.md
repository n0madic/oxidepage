# ADR-0031: The `Input` and `DOM` domains

- Status: accepted; **D3 superseded by ADR-0033**
- Date: 2026-08-03
- Builds on: ADR-0023 (trusted input), ADR-0026 (transform-aware geometry), ADR-0027 (browser, contexts, commands), ADR-0030 (CDP transport and the remote object model)
- Constrained by: design §2 (P6 "absent beats fake", P7 "conformance is automated"), §5.2 (generation-checked node ids)

## Context

ADR-0030 landed the transport and the remote object model, and `cargo xtask
puppeteer` scored 20 of 27. All seven failures had one of two causes: six
(`page.$`, `page.$$`, `page.$eval`, `page.waitForSelector`, `page.click`,
`page.type`) needed the two missing domains, and the seventh (`page.content`)
needed an `XMLSerializer`.

Almost nothing new was needed *below* `page`. Stage 2 (ADR-0023) built
`Page::dispatch_mouse`/`dispatch_key`/`insert_text`/`dispatch_wheel`, and stage 4
(ADR-0026) built `Page::content_quads`/`layout_rect`/`scroll_into_view_if_needed`
— those doc comments already name the CDP methods they were written for. What
was missing was the protocol surface, a node identity that can cross a wire, and
the `PageHandle` wrappers to reach it.

Two things the plan expected to matter turned out not to. Puppeteer 24 does
**not** call `DOM.getContentQuads`, `DOM.getBoxModel` or
`Page.getLayoutMetrics`: `clickablePoint` is a pure in-page `evaluate` of
`getClientRects()`. Those three ship anyway, because the roadmap lists them and
Playwright (stage 10) uses them, but they are not what unblocks the milestone.
And `Page.addScriptToEvaluateOnNewDocument` — which the roadmap lists under this
stage — had already landed in stage 6.

The load-bearing pair is `DOM.describeNode` + `DOM.resolveNode`. Nearly every
`ElementHandle` method carries the `bindIsolatedHandle` decorator, which runs
`describeNode({objectId})` → `resolveNode({backendNodeId, executionContextId})`
and transfers results back the same way. That single mechanism is why all six
checks failed together.

## Decision

### D1 — `backendNodeId` is a registry that carries the whole generation

The roadmap mandates that the mapping carry the generation. It **cannot** be a
bit-pack. CDP node ids are JSON *integers* that a driver round-trips through
`JSON.parse`, exact only below 2^53. `NodeId` is `{u32 index, NonZeroU32
generation}` = 64 bits, and the generation genuinely uses its range:
`Arena::free` bumps it and `DomTree::with_generation_base` seeds each new
document above the outgoing arena's high-water mark. So `generation << 32 |
index` rounds away the **low** bits — the index — and a corrupted token names a
*different live node* with no error anywhere. That is strictly worse than not
carrying the generation at all.

A lossy-generation variant (index exact, an N-bit generation "witness"
re-checked against the arena) is tempting and stateless, but it carries only
part of the generation and leaves a residual aliasing window. Neither the
roadmap's wording nor CLAUDE.md's absolutism about generation-checked ids
authorizes that trade.

`crates/page/src/node_handle.rs` is therefore a table, which carries the
generation *literally*: the handle is an opaque monotonic counter and the
`NodeId` behind it keeps its own generation, so `Arena::get` is the check.
Four properties are deliberate:

- **`by_node` is load-bearing, not an optimization.** Every `bindIsolatedHandle`
  round trip calls `describeNode`, so without one stable handle per node the
  table would grow per *call* instead of per *distinct node*.
- **It does not pin.** A handle naming a collected node must fail; pinning would
  turn a driver's node cache into a document-lifetime leak.
- **It lives on `Page`, not `PageState`.** ADR-0030 D3 put `ObjectStore` on
  `PageState` because it holds `!Send` `JsValue`s that must drop before the
  realm. This table holds plain data, so that argument does not transfer and
  `bindings` should not learn about a table it never reads.
- **It clears on navigation**, beside `state.reset_for_navigation()` and
  `render.reset()`: every handle named a node of the outgoing document.

The cap is `MAX_NODE_HANDLES = 100_000`, sized against `DOM.getDocument {depth:
-1}` on a genuinely large document. Past it, `Page::node_handle` first sweeps
entries whose `NodeId` no longer resolves — a dead node's handle could not be
resolved anyway, so dropping it loses nothing — and reports
`RemoteError::OutOfHandles` only if the sweep frees nothing — never handle `0`,
which is the value reserved for *no node* and would name descriptions that look
valid and can never be addressed.

### D1a — Producing a description costs no stack; consuming one is capped

A node description is built for a tree of arbitrary depth, and *four* passes
walk it: the build, the protocol layer's JSON construction, `serde_json`'s
serializer, and the nested value's own recursive `Drop`. Written recursively,
`DOM.getDocument {depth: -1}` against
`document.body.innerHTML = '<div>'.repeat(12000)` is a native stack overflow —
an abort of the whole endpoint process, reachable from page content. That was
measured, not hypothesised.

The build is therefore an **explicit-stack loop** (`Page::describe_tree`), the
same rule `dom::serialize` states and `xml_serialize` follows. That alone does
not cover the three consumers, so the *result* is capped at
`MAX_DESCRIPTION_DEPTH` (1000) and `depth: -1` means that cap. Truncation is not
a lie: `child_node_count` still reports the real number of children at the
boundary, so a driver can re-root a second `describeNode` there and continue —
which is exactly why CDP carries the count separately from the children.

### D1b — The engine's own per-level recursion is deferred, not fixed

Fixing the description path exposed the same shape underneath it: the load path
recurses once per DOM level too, in `layout::construct::build_box`, in taffy's
`compute_layout` and in stylo's traversal — the latter two by design. This
stage does **not** address it, and the reasoning is worth recording so the
question is not re-opened from scratch.

Measured on a 2 MiB page thread, loading nested `<div>`s with layout and paint
forced:

| Build | Outcome |
|---|---|
| debug | aborts at roughly **300** nested elements |
| release | survives **25,600**; past that it only gets slow (22 s), never overflows |

So it is a genuine unbounded recursion, but not a release-build defect at any
depth a real page reaches — 25,600 is some two orders of magnitude past the
deepest real DOM, and a document that deep is already unusable for unrelated
reasons. What is left is a **debug-build footgun**, which is exactly how it was
found: a test that builds a deep tree aborts under `cargo test` while the same
content is fine under `oxidepage serve`.

Making our own traversal iterative would not fix it while taffy and stylo still
recurse, so the bounded version of that fix does not exist — it would mean
forking two vendored engines. The fix browsers actually ship is a **nesting cap
in the parser** (Blink ~512, Gecko ~200), which is one place in `dom` but
changes DOM and parsing semantics, so it wants its own ADR and a WPT pass. That
is a robustness change, not a protocol change, and attaching it here would have
made this stage about something else.

Deferred, therefore, with two things that would move it: a nesting cap becoming
necessary for its own sake, or debug-build aborts becoming a recurring cost in
the test suite — at which point the cheap interim step is a `stack_size` on the
page thread (`engine/src/page.rs`), which raises the debug ceiling to match
release without pretending to remove the recursion.

### D2 — `nodeId` == `backendNodeId`, and `DOM.enable` means one real event

Chrome keeps two id spaces because it *pushes* a node tree to the client
(`DOM.setChildNodes`, `childNodeInserted`, …) and `nodeId` is the client's
cursor into that push. The roadmap puts DOM mutation events explicitly out of
scope — they are inspector features, not automation ones — so a second space
would exist solely to be a second name for the same node. Nothing in CDP
requires them disjoint.

`DOM.enable` therefore sets a `DomainFlags::dom` bit whose one consequence is
real: on `NavigationEventKind::Committed`, `crates/cdp/src/pump.rs` emits
**`DOM.documentUpdated`** to sessions carrying the flag. That is the honest
signal that every issued id is dead, it is exactly true of this model (a new
arena, generations seeded above the old high-water mark), and it is the one DOM
event here with real content rather than a stub.

If Playwright (stage 10) turns out to rely on the `DOM.setChildNodes` push
rather than on `getDocument` + `querySelector`, this is the decision to revisit
— then, with a real driver in front of it, not now on speculation.

### D3 — `DOM.resolveNode`'s `executionContextId` is validated, then ignored

**Superseded by ADR-0033**: the id now selects the world the handle is minted
in, so the asymmetry with `Runtime.evaluate` described below is gone — both
route by context id. The validation argument survives unchanged and is why a
stale id is still an error rather than a silent alias.

ADR-0030 D8 keeps one world named twice: a named world reports `base +
ISOLATED_WORLD_ID_OFFSET + index` but acts on the main world. `resolveNode`
resolves into the main world whichever id it is handed — without that,
Puppeteer's `adoptBackendNode`, which always passes the *utility* world's id,
would fail on every query.

But it **validates before ignoring**: the id is checked against
`session.page.execution_context_id()` and against `world_context_id(base, i)` for
each announced world. A *stale* id — one minted before a commit — silently
accepted would hand back a handle into the new document while the driver
believes it names the old one: a cross-document alias, the exact failure D1
exists to prevent. An id matching neither is `server("Cannot find context with
specified id")`; absent means the main world.

This is stricter than `Runtime.evaluate`, which accepts and ignores `contextId`
outright. The asymmetry is deliberate and is recorded here rather than
smoothed over: `evaluate` hands back a value computed *now*, while `resolveNode`
hands back a name for a node that may predate the commit.

### D4 — The refusal register

One rule, applied consistently — a refinement of ADR-0030 D9, which scoped
itself to *overrides*:

> **`method_not_found`** when the *capability* is absent from the engine
> entirely — there is nothing to withhold and a driver can feature-detect.
> **`server()` naming the reason** when the capability exists or is scheduled,
> so the driver's error message is actionable.

| Method | Answer |
|---|---|
| `Input.dispatchTouchEvent`, `Input.dispatchDragEvent`, `Input.setInterceptDrags`, `DOM.getFrameOwner` | `method_not_found` — no touch events, no `DataTransfer`, no nested browsing contexts to withhold. Puppeteer's `elementHandle.scrollIntoView` catches a protocol error and falls back in-page, which is the proof this register is right. |
| `DOM.setFileInputFiles` | `server("… Blob/File, input.files and the file-chooser path land with request interception")` — `<input type=file>` exists; the capability is scheduled, not absent. |
| `DOM.describeNode` on an `objectId` that is not a node | `server("Node with given id does not belong to the document")` — never a panic. |
| `Input.dispatchKeyEvent` with neither a `key` nor a resolvable `code` | `invalid_params` |

`DOM.focus` is **not** implemented: Puppeteer's `elementHandle.focus()` is an
in-page `el.focus()`, and the roadmap does not list it.

### D5 — The key table is the single source of `code` and `keyCode`

`Input.dispatchKeyEvent` carries `windowsVirtualKeyCode`. It is accepted and
**not** used to override the table: honouring a driver's number would let
`KeyboardEvent.code` and `.keyCode` disagree about which physical key was
pressed, which no real keyboard can do.

`code` is the opposite case and is honoured, because it is a *different axis*
from `keyCode` and the driver is authoritative on it. The table stores `Shift`,
`Control`, `Alt` and `Meta` once each, so it can never produce `ShiftRight` on
its own — and a page whose shortcut handler branches on `e.code`, the
recommended layout-independent idiom, would never fire. `key_for_code` knows
the right-hand and keypad twins for the same reason, so a `code`-only dispatch
of `ShiftRight` resolves rather than being refused.

`keys::key_for_code` is the reverse of `keys::lookup`, built over the *same*
two sources — the `NAMED` table, then the codes `printable_code` emits — so the
two cannot drift. A code neither knows (numpad, media keys) is `None` and the
command is `invalid_params`, because synthesizing a `code` the table would never
produce is a lie about the keyboard.

`KeyInput` gained `text: Option<&'a str>` and `location: u32`. `text` lets a
driver's own answer win over the US-layout table, and `Some("")` is meaningful
and *not* the same as `None`: it says "this key types nothing", suppressing both
`keypress` and the text-editing default action. `rawKeyDown` deliberately passes
`None` rather than `Some("")` — the table already yields no text for exactly the
keys Puppeteer sends `rawKeyDown` for (`Backspace`, `Tab`, the arrows), so
letting it decide is identical in effect *and* leaves their default actions
intact. `location` replaces a hardcoded `0` and fixes `ShiftLeft`/`ShiftRight`
for free.

### D6 — Every indexed-getter interface is iterable

`page.click` and `page.type` remained red after both domains were complete, with
"value is not iterable" from inside Puppeteer's own code. The cause was an
engine bug the new coverage surfaced: `clickablePoint` is
`[...element.getClientRects()]`, and `DOMRectList` had no `@@iterator`.

WebIDL says an interface with an indexed property getter and no `iterable<>`
declaration still exposes `@@iterator` = `%Array.prototype.values%`. The
`install_value_iterators` list held only `NamedNodeMap` and `HTMLCollection`, so
`DOMRectList`, `CSSStyleDeclaration`, `StyleSheetList`, `CSSRuleList`,
`PluginArray` and `MimeTypeArray` all threw on a spread. The rule is uniform, so
the fix is the whole set rather than the one that happened to be noticed —
leaving the other five broken would have been arbitrary.

That judgement paid immediately: the WPT run flipped five subtests from FAIL to
PASS — `CSSStyleDeclaration-iterator.html` directly, plus four
`serialize-all-longhands` / `getComputedStyle-logical-enumeration` /
`cssstyledeclaration-csstext-all-shorthand` subtests that enumerate a style
declaration by spreading it. Fixing only `DOMRectList` would have left all five
red.

### D7 — A triple click selects the line, and only where the line is knowable

Driving the finished endpoint by hand surfaced the last gap: `page.click(sel,
{clickCount: 3})` followed by `page.type` — the idiom every driver uses to
*clear and retype* a field — appended instead of replacing, because repeated
clicks changed no selection. `page.type` was doing exactly what it was told;
nothing had selected the text it was supposed to replace.

A triple click selects the **line** under the pointer, and identifying that line
in general needs a character-level hit test the engine does not have. Rather
than approximate it, `select_on_multi_click` acts only where the answer is
*exact*: a control whose value holds no line break has exactly one line, so the
line under the pointer is the whole value wherever the pointer was. That covers
every single-line `<input>` — the whole of the driver idiom — and a `<textarea>`
that happens to hold one line.

The two cases that genuinely need the offset are left alone: a **double** click
(which selects a word) and a triple click in a **multi-line** `<textarea>`.
Selecting the last word, or the whole value, would be confidently wrong text —
worse for a driver than seeing that nothing was selected, because wrong text
gets typed over.

The path routes through `text_selection::select`, so a triple click and
`el.select()` cannot disagree about direction or clamping, and it runs *after*
the focus transfer — focus collapses the caret to the end of the value, so
selecting first would undo itself.

## Consequences

`cargo xtask puppeteer` is **33/33 with an empty expectation file**: the seven
failures ADR-0030 named are gone, and six new checks (`page.hover`,
`page.select`, `page.$$eval`, `elementHandle.boundingBox`,
`page.keyboard.press`, `page.mouse.wheel`) pass. Milestone "Puppeteer
interaction green" is met.

The layering held. `page` gained a protocol-neutral node surface
(`crates/page/src/domnode.rs`, sibling of `remote.rs` under the same module-doc
rule: nothing there knows what CDP is) and learned nothing about protocols.
Three primitives moved *down* rather than being copied: `node_name` and the
`nodeType` mapping are now `oxidepage_dom::{node_name, NodeKind::node_type}`,
because they are DOM concepts and the protocol needs them without entering JS.
`LayoutEngine::box_quads` is new and computes all four CSS boxes in one pass over
`border_frame` — real quads, not four bounding boxes, and a `width`/`height`
that stay the *untransformed* used values for the same reason `offset*` does
(ADR-0026).

`XMLSerializer` was pulled in to close `page.content`: `crates/dom/src/
serialize.rs` grew `xml_serialize`, over the same explicit-stack traversal the
HTML serializer uses, and the IDL interface sits beside `DOMParser`.

Two deviations from the plan are worth naming. `PageHandle::document_description`
returns `Option<NodeDescription>` rather than the bare struct — the `None` arm is
unreachable (the document is always arena slot `(0, generation 1)`) but inventing
a default node would have been fakery, so the CDP layer turns it into a real
error. And `PageHandle::dispatch_mouse` is an ordinary `with` job under the
default `command_timeout`, not a `call_within` with a longer deadline: a click
that navigates is bounded exactly as `PageHandle::navigate` is, and making a
click the more patient of the two would be incoherent.

**Verification.** `crates/layout/tests/geometry.rs` (box quads, transformed and
not), `crates/page/tests/input.rs` (explicit `text` override, empty-text
suppression, `location`, triple-click selection and the two cases it declines), `crates/page/tests/domnode.rs` (handle stability, a
collected node, a handle across a navigation, descriptions of every node kind,
object round trip, selectors, depth truncation on a tree that used to abort the
process) and `node_handle.rs`'s own unit tests (the cap, the sweep, monotonic
handles), `crates/engine/tests/input.rs` (the command
boundary, including a navigating click over loopback),
`crates/cdp/tests/{input,dom}.rs` (the wire), `crates/bindings/tests/bindings.rs`
(`XMLSerializer`), and `cargo xtask puppeteer` as the acceptance gate. WPT is
unchanged apart from the five subtests D6 fixed; goldens and reftests are
unchanged.

## Deliberate limits (P6 — absent beats fake)

- **No DOM mutation events over the protocol.** `DOM.setChildNodes`,
  `childNodeInserted`, `childNodeRemoved`, `attributeModified` and friends are
  absent. They exist to keep an inspector's mirrored tree in sync; an automation
  driver queries. `DOM.documentUpdated` is the one event implemented (D2).
- **A node never carries `frameId`.** Puppeteer's `contentFrame()` returns
  `null` iff `typeof node.frameId !== 'string'`, and `null` is the correct
  answer until nested browsing contexts exist (stage 11).
- **`Input.dispatchKeyEvent`'s `commands` are ignored.** The macOS editing
  commands (`selectAll`, `moveToEndOfLine`, …) are declared and dropped; the
  key's own default action still runs.
- **No numpad or media keys.** `key_for_code` answers `None` for a code the key
  table has no key for, and the command is refused rather than served with an
  invented `code` (D5). F-keys and the named editing keys are covered.
- **No `DOM.focus`, `DOM.getOuterHTML`, `DOM.setAttributeValue`,
  `DOM.removeNode`, `DOM.getSearchResults`.** Every one has an in-page
  equivalent a driver already uses, and none is on the roadmap.
- **No touch or drag input.** `Input.dispatchTouchEvent`,
  `dispatchDragEvent` and `setInterceptDrags` are `method_not_found`: there are
  no touch events and no `DataTransfer` to drive them with.
- **`DOM.setFileInputFiles` is scheduled, not absent** — it answers `-32000` with
  the reason, and lands with stage 8's request interception.
- **`XMLSerializer` does not generate namespace prefixes.** The spec's prefix
  map, invented `xmlns` declarations and well-formedness errors are not
  implemented; stored prefixes and declarations are emitted as-is. Enough for
  `page.content()`'s doctype and for round-tripping a document that already
  carries its own declarations.
- **A double click selects no word, and a triple click in a multi-line
  `<textarea>` selects no line** (D7). Both need the text offset the pointer
  landed on; there is no character-level hit test. A single-line control is
  exact and is selected.
- **A selection change fires no `select` event.** Pre-existing: neither
  `setSelectionRange()`, `select()` nor now a triple click queues one, so the
  mouse path stays consistent with the script path rather than becoming the one
  place it fires.
- **A node description is at most `MAX_DESCRIPTION_DEPTH` (1000) levels deep**,
  and `depth: -1` means that (D1a). `childNodeCount` at the boundary stays
  truthful, so a driver re-roots and continues.
- **The engine's own load path still recurses per DOM level** (D1b). Harmless in
  release to ~25,600 nested elements; a **debug** build aborts around 300, so a
  test that builds a deep tree fails under `cargo test` while the same page is
  fine under `oxidepage serve`. Deferred deliberately — the real fix is a parser
  nesting cap, which is a DOM-semantics change of its own.
- **`Page.getLayoutMetrics` reports no separate visual viewport.** There is no
  pinch zoom, so `visualViewport` and `cssVisualViewport` repeat the layout
  viewport at `scale: 1` rather than reporting numbers no code path can produce.
