# ADR-0018: Connected nodes retain their JS wrapper (expando preservation)

- Status: accepted
- Date: 2026-07-12

## Context

Design §5.3 states the wrapper contract: "Each DOM node gets at most one JS
wrapper per realm (wrapper cache: `HashMap<NodeId, WeakJsRef>`), so `===`
identity holds. Wrappers hold [state]." The cache is deliberately **weak**
(`Map<index, WeakRef<wrapper>>` in `bootstrap.js`) so that detaching a subtree
with no pinned nodes can free it, and so JS→Rust cycles cannot leak (a wrapper
pins its node; GC of the wrapper unpins it).

That weak cache silently breaks the guarantee for a node kept alive **only by
tree connectedness** — i.e. reachable through the document, but with no JS
reference to its wrapper. QuickJS may collect such a wrapper; the next access
mints a **fresh** one, dropping every author-set (expando) property and breaking
`===` identity. The node lives on, its wrapper state does not.

This is not a corner case. jQuery stores its data-cache id as an expando on the
element wrapper (`elem[jQuery.expando]`), and AngularJS (running on jQuery)
stores directive controllers through `element.data(...)`. On angularjs.org the
`appSource` directive `$compile`s a `<uib-tabset>`; the tabset controller is
written onto the connected root element, but its wrapper is collected before the
tab panes link, so `require: '^uibTabset'` throws `$compile:ctreq` and the tab
content never transcludes. The failure is load-dependent ("the last tabset
works" — its wrapper is still hot), which is the signature of GC timing.

Browsers avoid this by wrapper-tracing: the GC walks the DOM and keeps a
reachable node's wrapper alive. The engine cannot trace its Rust-arena DOM from
QuickJS, so it needs an explicit rule. Full wrapper-tracing was deferred in the
design's risk table ("revisit with engine GC-tracing hooks if real workloads
demand it"); jQuery/Angular are that workload.

## Decision

A node's JS wrapper is **strongly retained while the node is connected**, in a
new `PageState::connected_wrappers: HashMap<NodeId, JsValue>` (mirroring
`custom_wrappers`, which already retains upgraded custom-element wrappers). This
restores the §5.3 guarantee — stable identity and surviving expando state — for
every connected node, exactly where the platform requires it. Retention is
dropped on disconnect, so detached subtrees still free as before.

Three edges drive the map:

- **Wrapper minted for an already-connected node** (`BindCx::node_to_js`):
  retain immediately, synchronously with creation. This is the common path
  (jQuery/Angular wrap an element already in the tree) and the one that fixes
  angularjs.org directly.
- **A pinned (wrapped) node crosses the connectedness boundary**
  (`DomTree::set_connectedness_composed`): the single connectedness hook records
  `(NodeId, connected)` on a `pinned_connectivity` queue — only while wrappers
  exist (`pins` non-empty). The bindings layer drains it, retaining on connect
  and releasing on disconnect.
- **The queue is drained synchronously at the host-call boundary**
  (`cx::native`, alongside the MutationObserver-microtask enqueue) and again in
  the event loop. The synchronous drain matters: a node created detached, then
  connected, then followed by allocation *in the same task* could otherwise have
  its wrapper collected before a deferred drain ran. The drain is a cheap no-op
  unless that call moved a wrapped node across the boundary.

`connected_wrappers` is cleared on navigation, like `custom_wrappers`.

## Consequences

- jQuery- and Angular-based pages work: `element.data(...)`, `.attr` caches, and
  every other expando-on-wrapper pattern survive GC. The `$compile:ctreq`
  mis-render on angularjs.org is gone; tab content transcludes.
- Memory for connected wrapped nodes is bounded by "nodes script has touched and
  that are still connected" — the same set a browser keeps wrappers for. It is
  released on disconnect and on navigation. Detached wrapped subtrees keep the
  prior lifetime (pinned until their wrapper is GC'd), so
  `detached_unwrapped_subtrees_are_freed_on_gc` and the detached-tree freeing
  path are unchanged.
- A node created detached, given expandos, and left detached is still subject to
  the weak cache — if its wrapper is collected before it is ever connected, the
  expandos are lost. This matches "a detached subtree is collectable" and is the
  rare case; the connect transition re-retains it the moment it enters the tree.
- The connectedness hook does a little more work when wrappers exist (one
  `pins` lookup per node in a connecting/disconnecting subtree), and the
  host-call trampoline drains a usually-empty queue. Both are gated to be free on
  pages with no live wrappers.
- Regression tests: `connected_node_wrapper_expando_survives_gc` (positive) and
  `disconnected_wrapped_nodes_are_freed_on_gc` (no-leak) in
  `crates/bindings/tests/bindings.rs`.
