# ADR-0004: Phase 3 network-layer implementation decisions

- Status: accepted
- Date: 2026-07-04

## Context

Phase 3 (design doc §5.5, §6, §8, §10) adds the network layer: the `net`
crate (SSRF connector, HTTP(S) client, cookies, cache, redirect/referrer
pipeline), the async↔sync page bridge, HTTP document loading with real script
timing (`defer`/`async`/module), ES modules, and the `URL`/`fetch`/`XHR`/
`document.cookie` bindings. Implementing it forced a set of decisions the
design document left open. This ADR records them.

## Decision

1. **The SSRF connector is the single enforcement point (§8).** A hand-written
   `tower_service::Service<Uri>` resolves DNS in-house
   (`spawn_blocking(to_socket_addrs)`), filters *every* resolved address
   through the policy, and connects only to vetted ones. IP literals and DNS
   names go through the same gate, so DNS-rebinding and numeric-literal
   bypasses close by construction. We deliberately do **not** use
   `HttpConnector::new_with_resolver` (it skips the filter for IP-literal
   hosts). TLS is layered on top with `hyper-rustls`: SNI verifies the
   hostname while the TCP connection went to the vetted IP — the SSRF-correct
   split. The address filter is hand-rolled with octet/segment checks (not
   the mostly-unstable `std::net` range predicates) so the blocked set is
   explicit and IPv4-mapped/compatible IPv6 forms recurse into the IPv4 rules.

2. **TLS provider is `ring`; roots are bundled `webpki-roots`.** This avoids
   the aws-lc-rs C/NASM toolchain that breaks Windows CI, and the bundled
   Mozilla roots are deterministic across the three-OS matrix. The rustls
   `ClientConfig` is built explicitly with `builder_with_provider` rather than
   relying on a process-default provider.

3. **The redirect/cookie/referrer/decompression pipeline is a hand-rolled
   `async fn`, not tower `Service` composition.** tower's `poll_ready`/`Layer`
   ergonomics fight per-hop redirect re-entry. Only the connector and the
   hyper client are tower (`tower-service` trait only). Each redirect hop
   re-enters the whole pipeline, so per-hop SSRF re-validation is free and
   `Location` targets can never smuggle a validated connection to an internal
   address.

4. **The async net ↔ sync page bridge is crossbeam-channel + `recv_deadline`.**
   `NetService` owns a multi-thread tokio `Runtime` on the page thread (the
   page is not a runtime worker, so `handle.spawn`/`block_on` are legal).
   net→page events flow over a `crossbeam_channel::Receiver<NetEvent>`;
   `Receiver::recv_deadline(Instant)` unifies "a net event OR the next timer
   deadline OR the settle budget" into one blocking wait — the crux that keeps
   timers and network both progressing with no busy-wait. page→net is a direct
   `handle.spawn` plus a per-request cancel flag. Synchronous needs (the
   document load, parser-blocking scripts, and the module loader) use
   `fetch_blocking` (`block_on`), which parks only the page thread while tokio
   workers deliver the bytes.

5. **ES modules use a synchronous `Loader` that blocks on the net.** The
   rquickjs `Runtime::set_loader` resolver/loader wrap a neutral engine-agnostic
   `ModuleSource` trait (implemented in `page` over the net stack), so `js`
   stays free of any HTTP dependency. `eval_module` does
   declare → `meta().set("url", …)` → `eval`, and the loader stamps
   `import.meta.url` on each nested module before returning it. This nested-meta
   path depends on a rquickjs 0.12.0 implementation detail, so `rquickjs` is
   pinned to `=0.12.0` and a module smoke test guards it.

6. **fetch/XHR promises are built with a JS bootstrap helper.** `makePromise()`
   → `{promise, resolve, reject}` (mirroring the existing `makeDomException`
   pattern) plus `resolvedPromise`/`recordPairs` avoid adding
   promise-construction methods to the engine trait. Net completions
   resolve/reject the stored functions inside a `with_scope` + microtask
   checkpoint, routed from the page's `dispatch_net_event` to the bindings'
   `deliver_net_event`. XHR events are dispatched directly to the handler
   properties and `addEventListener` registrations (a minimal `{type, target}`
   event object) rather than through the DOM `EventTarget` machinery — a
   deliberate Phase 3 simplification.

7. **Script timing is buffered, not truly streamed.** `fetch` buffers the full
   (decompressed) body; the document load feeds it to the parser via
   `push_input`. Parser-blocking external scripts fetch synchronously (blocking
   the parse); `defer` and module scripts run in document order after parsing,
   before `DOMContentLoaded`; `async` scripts run on arrival during the event
   loop. `load` waits for `async` scripts/subresources; `fetch`/XHR do not
   delay `load` but do keep `settle` running while in flight.

8. **Conformance is a local test server + Rust integration tests + the `url/`
   WPT subset.** Full upstream `wptserve` `fetch/`/`cookies/` conformance is
   out of scope for this phase (design §12). CI uses only loopback and never
   touches the real internet: the SSRF battery, cookie/referrer/cache unit
   tests, and the page HTTP-load/script-timing/module/fetch/XHR integration
   tests all run against in-process loopback servers. Iframes, workers, WASM,
   `<link rel=preload>`, and a disk HTTP cache remain deliberately absent.
   (`document.write` was absent when this ADR was written; parser-time writes
   are now supported — see design §12.)

## Consequences

- No `unsafe` is required anywhere in `net`; it keeps `unsafe_code = "deny"`.
- The engine trait grows only `set_module_loader`, `eval_module`, and
  `promise_state`; the network surface stays in `net`/`bindings`.
- Correctness never depends on the cache (§5.5): a stale entry is simply a miss.
- CORS is simple-requests-only; a non-simple cross-origin `fetch`/XHR is
  rejected under the default policy (preflight is Phase 10, §12).
