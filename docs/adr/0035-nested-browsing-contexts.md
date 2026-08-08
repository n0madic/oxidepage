# ADR-0035: Nested browsing contexts — one arena, N rendered documents

- Status: proposed
- Date: 2026-08-09
- Builds on: ADR-0017 (real `Document` nodes), ADR-0022 (navigation), ADR-0027
  (browser contexts), ADR-0030 (CDP), ADR-0033 (isolated worlds), ADR-0034
  (frame plumbing)
- Supersedes: ADR-0017 D4's "only one *rendered* document" invariant; ADR-0028
  D3's `node_document(el) == dom.document()` image gate; ADR-0034 D5's deferral
  of `Page.frameAttached`/`frameDetached`; ADR-0033's deferral of
  `Runtime.executionContextDestroyed`; ADR-0027 D12's deferral of `postMessage`
  and named targets; ADR-0022 §8's "there is one browsing context, so a `target`
  link navigates in place"
- Constrained by: design §2 P4/P5/P6/P7, §5.3, §7, §12

## Context

`docs/automation-roadmap.md` stage 11 is the last item on the plan and the
largest: "sites that hide content in an iframe". Today `HTMLIFrameElement` is an
empty interface, an `<iframe>` gets an ordinary `BoxKind::Block` and lays out its
DOM children like a `<div>`, and design §12 lists iframes as an explicit v1
limit.

The obstacle is not the element. It is the sentence CLAUDE.md states plainly —
"there are many documents, but only one *rendered* one". `NodeFlags::IS_CONNECTED`
means "in *the* rendered document" and gates style, layout, resource loading,
custom-element upgrades, the `getElementById` index and event bubbling to the
`Window`. ADR-0017 D4 chose that narrowness deliberately: it is what makes a
`DOMParser` document inert *structurally* rather than defensively, and it warned
that "widening it would turn a second document into a live one". Widening it is
precisely this stage's job — but it must widen to "in *a* rendered document, and
here is which one", never to "in some document".

Two shapes were available.

**One `DomTree` per frame** looks like the obvious isolation boundary and is
not viable. `enter_active_tree` (`crates/dom/src/select.rs:127-139`) refuses to
nest a *different* tree — the `debug_assert` is there because outer `NodeRef`
handles would silently resolve against the inner arena, and `NodeRef`'s
pointer-sized-handle soundness (ADR-0005) rests on that. Parent hit testing and
parent paint must descend into a child document on the same stack, so the
refusal is not incidental. Separate arenas also seed generations from
`FIRST_GENERATION` each, so `(index 5, gen 3)` in frame A aliases the same id in
frame B — the exact silent aliasing the generation checks exist to prevent — and
every layer that keys on a bare `NodeId` (wrapper cache, `NodeHandle`, CDP
`backendNodeId`, listener and observer registries) would have to carry
`(FrameId, NodeId)`, a change with no way to prove completeness.

**One arena holding N rendered documents** turns out to cost far less than it
looks. The style engine and the stylo glue are not structurally tied to arena
slot 0; they are tied to it by four literal call sites and two hardcoded
constants. Upstream stylo is fully root-relative: `RecalcStyle::pre_traverse`
and `driver::traverse_dom` take an arbitrary element, `TDocument` is four
methods, and only `shared_lock()` comes from a document at all — correctly
tree-global, since one `SharedRwLock` across N documents is what stylo wants.
`DomTree` already carries a `*_of(doc)` accessor family and a real `adopt`, so
cross-document node movement keeps wrapper identity for free. And because the
wrapper cache is keyed by arena index, a parent realm can wrap a child
document's nodes with no new machinery — which is exactly what
`iframe.contentDocument` needs.

## Decision

### D1 — One arena, N rendered documents; `IS_CONNECTED` becomes membership

`DomTree` gains `rendered_roots: HashSet<NodeId>`. `propagate_connectedness`
changes its parentless case from `node == self.document` to
`self.rendered_roots.contains(&node)`, and `add_rendered_root` /
`remove_rendered_root` are the only ways in and out — each running
`set_connectedness_composed` over the whole subtree so the id index, the pin
connectivity log and shadow trees stay correct.

`IS_CONNECTED` keeps meaning "in a rendered document"; what changes is that
there are now several. Every consumer that used the flag as a *boolean* must be
re-read as a *routing question* — "which engine, which frame" — because the
answer is no longer unique. That re-audit is the real cost of this decision and
it is enumerable rather than open-ended: `note_style_element_closed`, the script
push in `note_children_changed`, the id index in `set_connectedness_composed`,
custom-element upgrades, ADR-0028 D3's image gate (which becomes
`is_rendered_root(node_document(el))`, restoring the rule the flag was standing
in for), and bubbling to the `Window`. On the bindings side the checklist anchor
is `imp/document.rs::is_page_document`, which CLAUDE.md already names as the
"does this member reflect a browsing context" test: each of its call sites
becomes "route to *this document's* frame".

Documents with no browsing context — `DOMParser`, `createHTMLDocument`,
`new Document()` — are unaffected, because they are simply not in
`rendered_roots`. ADR-0017's structural inertness survives intact; it just stops
being spelled "is not slot 0".

The eleven remaining single-document sites are parameterization, not design:
`TNode::owner_doc`, `TDocument::is_html_document` and `quirks_mode` and
`is_html_document_body_element` in the stylo glue; the traversal root of
`StyleEngine::resolve_styles`; the `build_layout_tree` and `take_snapshot` roots
in layout; the `ids` index key; `url_extra_data`'s home; and the page crate's
update-queue routing. One of them is a correctness landmine rather than a
rename: `resolve_styles` ends with `tree.clear_snapshots()`, which is
tree-global, so with N engines the first to finish would destroy snapshots the
others have not consumed and silently lose their invalidation. It must filter by
document.

`style_version` and `structure_version` stay tree-global for now. That is a
performance cost — a mutation in frame B invalidates frame A's `ReflowStamp` and
forces a full box-tree rebuild — accepted deliberately, because per-document
counters are a contained follow-up and correctness does not depend on them.

### D2 — A frame owns its engines; `PageShared` splits

`crates/bindings/src/state.rs` documents `PageShared` as "everything that is
one-per-*document* rather than one-per-*world*". That was already the right
partition; there was simply one document. It becomes **`FrameShared`**, one per
browsing context, keeping `style`, `layout`, `pending_navigation`, `history`,
`referrer`, `parsing`, `ready_state`, `current_script`, `timing`,
`script_parser_buffer`, `pending_scroll_targets`, `storage_subscriber`,
`worlds` and the connectivity log, and gaining `document: NodeId` and
`frame: FrameId`.

What is left is genuinely page-level and moves to a new `PageGlobal`: the shared
`dom`, `hooks`, `navigator`, `screen`, the monotonic `next_context_id` and
`next_object_id`, `object_worlds`, `binding_calls`, `init_scripts`, `bindings`,
`net_world`, and the `Weak<dyn WorldEnter>`. `WorldState` holds one of each, so
the ~334 `cx.state.dom` / `.style` / `.layout` sites in `imp/` keep reading
identically.

A `LayoutEngine` bakes its viewport into the stylist's `Device`, and an iframe
needs its own viewport, so one engine pair per frame is a fit rather than a
workaround. `FrameId` lives in `oxidepage-base` beside `NodeId`, because
`bindings` needs it and cannot depend on `page`.

`WorldEnter` (`crates/bindings/src/state.rs:908`) grows the frame dimension —
`enter(frame, world, f)`, `world_ids_of(frame)`, `has_listener(frame, …)`. It is
the **only** path by which `bindings` reaches another frame's realm, which keeps
`postMessage` and `contentWindow` on the one audited edge rather than spread
across `imp/`.

### D3 — A realm per (frame × world), and frames are capped

Each frame gets its own main world and, on demand, its own isolated worlds; a
world is still a whole `rquickjs::Runtime` (ADR-0033 D1), so the count of
runtimes is `frames × worlds` and the JS memory ceiling multiplies with it.

ADR-0033 could leave `MAX_WORLDS` as a bound on a buggy *driver*, because page
script cannot create a world. Frames are different: page script creates them
freely, so the cap is a bound on a hostile *page* and has to behave like
ADR-0027's `max_pages_per_context`. Three limits: `MAX_FRAMES_PER_PAGE = 64`,
`MAX_FRAME_DEPTH = 10`, and HTML's matching-nested-browsing-context rule — a
frame whose ancestors already show the same URL loads `about:blank` and warns,
which is what stops `<iframe src="self.html">` from being an exhaustion
primitive.

### D4 — Cross-frame DOM is real; cross-frame JS object graphs are not

The shared arena makes `iframe.contentDocument` a real Document the parent realm
can walk, mutate and query — `node_to_js` wraps a node of any document, and the
wrapper it returns belongs to the *accessing* realm.

`contentWindow` cannot work the same way. A child frame's global lives in
another `Runtime`, and `Persistent::restore` compares runtime pointers, so
handing it to the parent is the leak ADR-0033 D1 exists to make impossible.
Instead `contentWindow` is a `WindowProxy` **in the accessing realm** —
extending the type `window.open` already returns (ADR-0027 D12) with a
same-thread backing, so `document`, `location`, `parent`, `top`, `frames`,
`length`, `name`, `postMessage`, `focus`, `blur` and `closed` are all real and
synchronous, and the getters that had to throw for a cross-thread sibling do not
have to here.

Two consequences are accepted and documented rather than worked around: an
object reached across a frame boundary carries the *accessing* realm's
prototypes, so `childDoc.body instanceof parentWindow.HTMLElement` is true where
a browser says false; and a child's globals (`contentWindow.myVar`) are not
reachable. Neither touches automation — a driver's `frame.evaluate()` runs
through CDP in the frame's own realm — and the alternative, one `Runtime` shared
by same-origin frames, is the multi-`Context` design ADR-0033 D1 rejected on
grounds that have not changed.

Origin comparison is `(scheme, host, port)`, as `pushState` already does
(ADR-0022 §4) — `Url::origin()` yields an opaque origin for `file:`, which is
wrong here. **`srcdoc` and `about:blank` frames inherit the embedder's origin**
rather than deriving one from their URL; without that rule the commonest
same-origin idiom there is would read as cross-origin.

`postMessage` reuses the `serialize_state` / `deserialize_state` pair
`history.rs` already uses for `SessionHistory` (ADR-0033 D3): serialize in the
sender's realm, deserialize in the receiver's. That is a JSON subset of
structured clone — no `Map`, `Set`, typed arrays, cycles or transferables — and
it is stated as a limit rather than approximated silently. Delivery is a
**task**, never a synchronous entry into the other realm, or a two-frame
ping-pong would ride the native stack until `MAX_WORLD_DEPTH` caught it.

### D5 — A frame load is a task source, and teardown is a checklist

Loading a frame's document from an attribute hook would run under live `RefCell`
borrows on dom and style, and `load_document` re-enters the event loop through
`await_subresources` — a deterministic `BorrowMutError`, the same reason
ADR-0022 §1 made navigation a task source. `PendingNavigation` therefore carries
a target `FrameId` and the loop drains per frame in pre-order.

`reset_frame_state` is the per-frame analogue of `reset_document_state`, and
every item on it is a leak or an abort if missed: abort **only** this frame's
in-flight requests (a `RequestId → FrameId` side table, since ids are unique
within a `NetService`) and cancel only its timers and rAF callbacks by
`WorldId`; tear down its worlds in reverse creation order, releasing each
world's values before any runtime is freed; emit
`Runtime.executionContextDestroyed` per context; drop the storage subscription;
`remove_rendered_root` and then `free_detached_tree_if_unpinned` — never an
unconditional free, because a parent-held `contentDocument` wrapper pins the
document (ADR-0017 D3); allocate the replacement with `create_document` plus
`add_rendered_root`; and rebuild the engine pair layout-before-style so
`StyleEngine` can take the new `LayoutEngine`'s font-metrics factory.
`DomTree::with_generation_base` has no role here — the arena lives on, and stale
ids die by the ordinary generation bump when their slots are freed.

Loading counters become per frame. `await_pending_stylesheets` waits on **its
own** frame only, because a child's render-blocking sheet must not block the
parent's scripts; `await_subresources` for the top document waits on the **sum**
over the tree, because `waitUntil: load` is meant to include the iframes.

### D6 — Reflow is a pre-order walk, and a child never feeds back

`Page::flush_layout` becomes a loop over the frame tree in pre-order. A parent
reflows first, which fixes its iframe box's content box; the child's viewport is
read from that box and its own reflow follows. Because an `<iframe>` is sized as
a replaced element — CSS and attributes, defaulting to 300×150, never its
content — the child cannot change the parent, so one pass suffices and there is
no iteration to converge.

The `dom` borrow is taken and released **once per frame**, never nested. That is
what keeps the loop free of `BorrowMutError`, and it is the reason the ordering
lives in `page` rather than as a callback from inside `LayoutEngine::reflow`.

### D7 — Paint splices the child's display list

`build_display_list` stays a whole-document function and `paint` stays a dumb
consumer (P5): `page` builds child lists first, post-order, and hands them down
through `PaintOptions`. At the iframe box, `paint_replaced` emits the child's
items between a `PushClip` on the content box and a `PushLayer` translating by
the content origin less the child's scroll.

Two details are load-bearing. `ImageId` is minted by a per-`ImageStore` counter
and every frame has its own store, so merging resource tables **must** rebase
image ids — getting it wrong yields silently wrong pictures, not a crash.
(`FontId` is content-hashed and merges as is.) And a child's scroll behaves like
*element* scroll, not document scroll: it is baked into the parent's list at
build time, so the parent's cache must be keyed on the descendants' stamps and
scrolls as well as its own. That fold lives in `page`'s `RenderState`, leaving
`PaintStamp` — and therefore `layout` — unaware that frames exist.

`position: fixed` inside a frame is the sharp edge. The child's list contains
`PushViewportAnchor` pairs, and splicing them unchanged is wrong twice over: the
anchor would suppress the *page's* scroll translation, when the iframe box itself
must move with page scroll; and the splice layer would make the fixed content
move with the *child's* scroll, when it must pin to the iframe's viewport.
Anchored segments are therefore re-emitted at the content origin with neither
the child-scroll term nor a page-level anchor, still inside the content-box clip.

Rasterizing the child to an image was the alternative. It is simpler and it is
what inline SVG does today, but it would put a bitmap in every PDF and turn text
into pixels in the display-list goldens, so it was rejected.

### D8 — Events stop at a document boundary; hit testing does not

An event's propagation path ends at its own frame's `Window`, per spec; `composed`
crosses shadow boundaries and does not change this. `EventTargetKey::Window` is
a unit variant today and becomes `Window(FrameId)`.

Hit testing does cross. `hit_box` already threads a point in each box's own
space with inverse transforms applied, so the crossing is a subtraction of the
border and padding at the iframe box followed by the child engine's own
`elements_from_point`, which applies the child's scroll itself. `page` drives
the crossing, because a `LayoutEngine` knows nothing of its neighbours. Input
synthesis reports `(FrameId, NodeId)`; a pointer leaving an iframe produces
`mouseout` in the child and `mouseover` in the parent as two chains, not one.

Focus stays page-global — one element is focused at a time — while
`document.activeElement` is derived per document, so an ancestor document
reports the `<iframe>` element. `:hover` needs one hop across the boundary so
the owning `<iframe>` matches while the pointer is inside it.

### D9 — CDP: frames get ids of their own

A target owns a tree of `Frame`s rather than one, which is the change
`crates/cdp/src/frame.rs` was split out to receive (ADR-0034 D5). `loader_id`
and `pending_loader` become honestly per frame. The main frame keeps the target
id — minting a new one would churn expectations for no gain — and children get a
fresh random id.

`Page.frameAttached` and `frameDetached` are implemented now that there is
something to describe. Per-frame ordering is a hard requirement, not a detail:
`frameAttached` → `frameStartedLoading` → `frameNavigated` → `lifecycleEvent`,
because Playwright's `_onFrameAttached` *creates* the object every later event
indexes into. `Runtime.executionContextCreated` fires per (frame, world) with
`auxData { frameId, isDefault }`, and `executionContextDestroyed` finally has a
sender. `DOM` gains node `frameId`, `getFrameOwner`, and `contentDocument` under
`pierce`.

ADR-0034 warned that a third caller of the two commit paths — the one that
renumbers execution contexts and the one that preserves them — "will produce a
driver that either drops every later event or rejects every command in flight".
A frame commit is that third caller, and it takes the renumbering path.

### D10 — Frame history is replace-only

A frame navigation replaces the frame's current entry and adds nothing to the
top-level history; `history.back()` inside a frame is a no-op that warns.

HTML's joint session history would have a top-level entry hold the state of
every frame, with traversal restoring all of them at once. That is a phase of
its own, it interacts with `MAX_HISTORY_ENTRIES` and with the absence of a
bfcache (ADR-0022 §3), and no part of the automation run reaches it. Stated as a
limit, not left ambiguous.

### D11 — `sandbox` implements the two tokens that mean something here

Without `allow-scripts`, the frame runs no script. Without `allow-same-origin`,
the frame gets an opaque origin, so `contentDocument` throws `SecurityError`.
Those two are enforceable and are enforced.

The rest — `allow-forms`, `allow-popups`, `allow-top-navigation`,
`allow-modals`, `allow-downloads` — are **not** implemented and do not pretend
to be: the attribute reflects, the unimplemented tokens are listed as limits, and
nothing silently claims a restriction it does not apply. Implementing the two
that work and rejecting the rest by name is P6; parsing all of them and
enforcing two would be the silent no-op it forbids.

## Consequences

The invariant in CLAUDE.md changes text: an arena holds N rendered documents,
one per browsing context, and `IS_CONNECTED` means membership in
`rendered_roots`. Design §12's "Iframes: not loaded" is struck.

Costs taken on deliberately:

- **Two shapes of "which document" now exist in the codebase** — membership in
  `rendered_roots`, and `node_document(x) == some_frame.document`. They answer
  different questions and a call site that picks the wrong one fails silently.
  The first asks "is this rendered at all", the second "is this *that* frame's".
- **Cross-frame prototypes come from the accessing realm** (D4). This is a real
  spec divergence and the first one in this engine that a same-origin page can
  observe directly.
- **A tree-global `structure_version` means cross-frame reflow interference**
  (D1). Correct, slower; per-document counters are the follow-up if a benchmark
  shows it.
- **The frame caps are a policy, not a spec** (D3). A page legitimately building
  65 frames will be refused, and told.

**Verification.** `crates/dom/tests/rendered_roots.rs` for the membership
predicate and for `DOMParser` documents staying inert; `crates/page/tests/frames.rs`
for loading (`src`, `srcdoc`, `data:`, 404 → `error`, nesting, recursive `src`,
the caps, style isolation both ways); `crates/page/tests/frame_scripting.rs` for
`contentDocument`/`contentWindow`, the same-origin checks, `srcdoc` origin
inheritance, `postMessage` both directions, and dropping a page with live frames
and worlds; `crates/page/tests/frame_input.rs` for click-through, `:hover` on the
owner, per-document `activeElement` and a transformed iframe;
`crates/layout/tests/construct.rs` for replaced-element sizing; a display-list
golden (`tests/goldens/iframe.html`), an Ahem reftest pair
(`tests/reftests/iframe-basic.html`) and iframe cases in
`crates/page/tests/screenshot.rs` and `pdf.rs` for paint, including a fixed
banner in a scrolled frame; `crates/cdp/tests/{page,runtime,dom}.rs` for the
frame tree, the event ordering and the `DOM` additions; and new iframe checks in
both `cargo xtask puppeteer` and `cargo xtask playwright`, expectation files
regenerated in the same commits as the behaviour changes.

## Deliberate limits (P6 — absent beats fake)

- **No out-of-process iframes and no frame targets.** One OS thread per page
  (design §7, ADR-0027 D1) and one session per page; a frame is not a `Target`.
- **A child's globals are unreachable from another frame** and cross-frame
  objects carry the accessing realm's prototypes (D4).
- **`postMessage` clones a JSON subset**: no `Map`, `Set`, typed arrays, cycles,
  transferables or `MessagePort`. `MessageChannel` is not installed.
- **`sandbox` enforces `allow-scripts` and `allow-same-origin` only** (D11).
- **No Permissions Policy (`allow`), no `csp` attribute, no per-frame CSP** — no
  CSP is enforced anywhere (ADR-0034 D6).
- **No `<frameset>`, `<frame>`, `<object>`, `<embed>`, `<portal>` or
  `<fencedframe>`.** `<iframe>` is the only embedder.
- **No `loading="lazy"` on iframes.** Every frame loads eagerly.
- **No COOP/COEP, no `crossOriginIsolated`, no `document.domain`.**
- **No `unload` or `beforeunload` on frame detach** — neither event exists
  (ADR-0022), so a frame's outgoing document observes nothing.
- **No joint session history and no bfcache**; a frame navigation is
  replace-only (D10).
- **No scrollbars in frames**, as nowhere else (design §12). A frame's overflow
  scrolls through the same clamped offsets the document does.
- **Events do not cross a document boundary** (D8), so a listener on the parent
  sees nothing a child dispatches. `postMessage` is the crossing that exists.
