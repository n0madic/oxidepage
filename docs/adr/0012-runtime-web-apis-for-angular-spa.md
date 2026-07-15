# ADR-0012: Runtime web APIs to unblock the Angular/SPA runtime (v1)

- Status: accepted; **D4 superseded by ADR-0017**
- Date: 2026-07-11

## Context

After ADR-0011 the five feature-detected APIs were present, but the target SPA's
Angular app still did not run correctly. Live tracing on 2026-07-11 (a temporary
`console.error` instrumentation that prints an `Error`'s `.stack`) found a chain
of *runtime* gaps that only surface once the app bootstraps, hydrates, and makes
its own DOM/network calls — each one masking the next:

1. **`document.createNodeIterator` / `createTreeWalker`** — Angular hydration
   walks the server-rendered comment markers (`ngetn`/`ngtns`) via a
   `NodeIterator` over `SHOW_COMMENT`. Missing → `TypeError: not a function`,
   the "Error while fetching data from API" the app reported.
2. **`document.implementation.createHTMLDocument` + `DOMParser`** — Angular's
   `DomSanitizer` builds an inert document to sanitize untrusted HTML.
3. **`Element.setAttributeNS` / `removeAttributeNS`** — Angular's renderer sets
   namespaced attributes (`xlink:href` on SVG icons); only the `get`/`has`
   variants existed.
4. **`Response.body` (a `ReadableStream`)** — Angular's fetch backend reads every
   response through `response.body.getReader()`. Without it each *runtime* fetch
   returned an empty body; SSR hydration survived only because its data rides
   `TransferState` in the HTML, not the fetch body. This was the real cause of
   the "Unable to find icon" cascade (the Material icon-set sprite fetched 200 OK
   but was read as empty).

Alongside these, general SPA/runtime primitives were still missing and are added
here: `crypto` (`getRandomValues`/`randomUUID`), `TextEncoder`/`TextDecoder`,
`requestIdleCallback`, Web Storage (`localStorage`/`sessionStorage`), and the
History API (`pushState`/`replaceState`/`popstate`). Purely
advertising/tracking-driven gaps (`Blob` for OpenReplay, `navigator.sendBeacon`,
`PerformanceObserver`) are deliberately **not** implemented.

## Decisions

**D1 — Prefer pure JS over the existing native DOM; add native code only for
capabilities JS cannot reach.** `NodeIterator`/`TreeWalker`, `DOMParser`/
`document.implementation`, Web Storage, `TextEncoder`/`TextDecoder`,
`requestIdleCallback`, History, and `Response.body` are implemented in
`bootstrap.js`'s `installLateGlobals`, over the already-exposed `Node`
navigation, `createElement`/`innerHTML`, timers, and `fetch`. Rationale, beyond
avoiding codegen churn: the traversal filters (`NodeIterator`/`TreeWalker`
`acceptNode`) are JS callbacks that would have to run **inside** a live DOM
borrow if the walker were native — exactly the borrow-discipline hazard the
engine forbids. Keeping the algorithm in JS walks wrapper objects, never the
arena under a borrow. This matches the established pattern (`structuredClone`,
`queueMicrotask`, the collection/style proxies).

**D2 — Exactly two native helpers, installed before `installLateGlobals` and
deleted after capture.** `__oxide_randomBytes(n)` returns `n` bytes from the OS
CSPRNG (`getrandom`) as a plain array, so `crypto.getRandomValues`/`randomUUID`
never need native typed-array access. `__oxide_setDocumentUrl(url)` replaces the
document URL in place (no navigation) for `history.pushState`/`replaceState`,
returning `false` on a cross-origin URL so the JS side throws `SecurityError`.
Same-origin is compared by the `(scheme, host, port)` tuple rather than
`Url::origin()` — `file:` and other opaque-origin URLs get a fresh unique origin
per parse, which would spuriously fail for local documents. The bootstrap
captures both into locals and `delete`s them from the global so page script never
sees the `__oxide_*` surface.

**D3 — `Response.body` is a one-shot byte `ReadableStream` backed by
`arrayBuffer()`.** The fetch backends only need `body.getReader()` +
`reader.read()` in a loop; the whole body arrives in a single `read()` (no
backpressure, no chunking). A minimal `ReadableStream` global (constructible from
a `{ start(controller) }` source that enqueues then closes) is exposed for
feature detection. This is the single highest-impact fix: it unblocks *all*
client-side data fetching in fetch-backend SPAs, not just icons. `Response`
`.clone()`/`.blob()` remain unimplemented (v1).

**D4 — Inert documents are shims over the main document, not real Documents.**
*(Superseded by ADR-0017: they are real `Document` nodes now, and `DOMParser`
does a real full-document parse. The behavioural guard for this decision —
`bindings.rs` `dom_parser_and_implementation_inert_documents` — still passes
unedited.)*
`createHTMLDocument`/`DOMParser` return a plain object exposing the members
sanitizers and parsers use (`body`/`head`/`documentElement`/`createElement*`/
`querySelector*`/`title`/`implementation`), with real but *detached* elements
created through the live document and populated through the existing `innerHTML`
parser. Parsing is body-first (Angular's sanitizer and the Material icon registry
pass body-level HTML / inline SVG); a full-document string's head-only content is
approximate. The engine has a single live document, so a second real `Document`
was out of scope.

**D5 — `setAttributeNS`/`removeAttributeNS` are native, matching the DOM
attribute model.** They run the "validate and extract" algorithm
(`InvalidCharacterError` on bad names, `NamespaceError` on prefix/namespace
mismatches) and address an existing attribute by `(namespace, localName)`
ignoring its stored prefix — the identity the DOM attribute list matches on — so
an update replaces in place and a remove finds the attribute regardless of prefix.

**D6 — Storage and History keep no persistence.** `localStorage`/
`sessionStorage` are in-memory per page (a `Proxy` over a backing `Map` for the
named-property surface); there is no `StorageEvent` or quota. History is a JS
session stack: `pushState`/`replaceState` update the URL in place via D2;
`go`/`back`/`forward` walk the stack and dispatch a synthesized `popstate`
`Event` (no reload, no real navigation).

**D8 — Leaf host-object proxies use the target as the fall-through receiver.**
`collectionProxy` and `styleProxy` traps now call
`Reflect.get/set(target, prop[, value], target)` rather than `…, receiver)` (the
proxy). A host object's native accessor brand-checks its `this` and rejects the
wrapping proxy on that path — Angular Material reads
`element.querySelectorAll(selector).length` while caching an icon's external SVG
references, and the renderer writes `element.style` in change detection.
Collections and `CSSStyleDeclaration` are leaf host objects, so the receiver is
not otherwise observable (indexed, named, and CSS-property access are handled
before the fall-through). This clears the visible icon-caching error; one
further "RustClass object expected" report remains — thrown by rquickjs at a
Rust↔JS boundary with no JS stack (so untraceable from script), inside a
microtask, non-fatal (icons still render). A deeper follow-up.

**D7 — The per-page network budget is CLI-configurable, and the CLI's own
default is higher than the library's.** `--max-bytes <size>` (e.g. `1G`, `2G`)
and `--max-requests <n>` override `ResourcePolicy`'s `max_total_bytes` and
`max_requests` (500) on every command. A headless renderer of content-heavy real
sites fetches *every* image eagerly (nothing lazy-loads), so the library's
conservative 256 MiB starves the tail of the page of its fonts and logos; the
CLI therefore defaults to **512 MiB** while embedders keep the 256 MiB default.
The SSRF and request-count defaults are untouched.

## v1 limitations

- `crypto.subtle` is not implemented.
- `TextEncoder`/`TextDecoder`: UTF-8 only; `encodeInto`'s `read` count is
  approximate on a partial fill.
- `NodeIterator` does not perform the spec's live "pre-removing" reference-node
  adjustment when a node is removed mid-iteration (no DOM-mutation hook);
  `TreeWalker` is unaffected (it reads `currentNode` lazily).
- ~~Inert documents are shims (D4): body-first parsing; not separate
  `Document`s.~~ Lifted by ADR-0017.
- `Response.body` is a one-shot stream (whole body per `read()`, no
  backpressure); `Response.clone()`/`Response.blob()` are absent.
- Web Storage is in-memory (no persistence/`StorageEvent`/quota); History
  `go`/`back`/`forward` never reload or navigate.
- `Blob`/`File`/`FileReader`, `navigator.sendBeacon`, and `PerformanceObserver`
  are intentionally omitted — their only observed consumers are analytics /
  tracking scripts.

## Consequences

The target SPA's Angular app now bootstraps, hydrates, fetches its runtime data, and
renders the real page (previously the mega-menu overlapped all content). The
console is clear of functional errors; the only remaining messages come from
advertising/tracking scripts (OpenReplay's `Blob`, a lazy ad chunk's warning),
which are out of scope. The `Response.body` stream additionally unblocks runtime
data fetching for any fetch-backend SPA.
