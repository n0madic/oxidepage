# ADR-0024: `XMLHttpRequest` conformance

- Status: accepted
- Date: 2026-07-27
- Builds on: ADR-0023, which made `XMLHttpRequest` a real `EventTarget` and
  recorded the remaining gap — "`ProgressEvent`, `xhr.upload`, and the
  `progress`/`timeout` events" — that this ADR closes.
- Constrained by: ADR-0004 (the net layer buffers whole response bodies).

## Context

ADR-0023 closed XHR's *identity* gap: its listeners moved into the shared
registry and its events became real `Event` objects. It left the *behavior* at
roughly a 2008 subset — 19 of ~28 members, five of eight events, and a state
machine wrong in ways a page can observe.

An audit found four outright bugs, not just missing members:

- **A reused XHR fired nothing.** The self-referential `wrapper` root — the
  thing that keeps a script-abandoned but in-flight request alive — was cleared
  on every terminal transition and never restored, while `fire_event` returned
  early when it was absent. So `x.open(); x.send()` after a completed request
  delivered **zero** events. The `XhrData::wrapper` doc comment claimed such
  events arrived with an undefined target; they did not arrive at all.
- **`open()` did not terminate an in-flight request.** The old request kept
  writing `Headers`/`Chunk`/`Done` into the reopened object.
- **`send()` twice started two requests**, both writing to one `XhrData`; only
  the second id was retained, so `abort()` could cancel only one.
- **`abort()` and the error path never reset the response.** After aborting a
  request whose headers had arrived, `status` was still `200` and
  `responseText` still held the partial body.

And one disclosure bug: **`Set-Cookie` was readable from script.** The net layer
forwards the whole header map for a same-origin (`ResponseType::Basic`)
response — correctly, since the document loader needs it — and neither
`getResponseHeader` nor `getAllResponseHeaders` applied Fetch's
forbidden-response-header filter. A cookie the jar treats as `HttpOnly` was one
`xhr.getAllResponseHeaders()` away from any script on the page.

## Decision

XHR now matches the living standard except for four absences, each stated below
rather than silently approximated.

### The state machine models the spec's flags, not the readyState

`XhrData` carries the **send() flag**, the **upload complete flag** and the
**response object cache** explicitly. That is what the transitions are written
against: a second `send()` is an `InvalidStateError` because the send() flag is
set, not because of a readyState value that a terminal transition may already
have moved on from.

- `open()` terminates any in-flight fetch, resets the response to a network
  error, clears the send() flag, **re-roots the wrapper**, then enters OPENED
  and fires `readystatechange` — but only when the state was **not already
  opened**, which is the spec's own condition on step 11. `open(); open()` is
  one transition, and code driving a state machine off `onreadystatechange`
  counts them. It also parses the URL and normalizes/validates
  the method, as the spec does — so a bad URL throws `SyntaxError` from
  `open()` rather than a `TypeError` from `send()`.
- `abort()` and the error and timeout paths share one "request error steps"
  helper: response → network error, state → DONE, exactly one terminal event,
  then `loadend`. `abort()` fires **only** when a request was in flight; it used
  to fire a full sequence at an XHR that had never been sent.
- The first body chunk enters **LOADING**. Nothing wrote readyState 3 before.

**The self-root is released only when the send() flag is clear.** A `load`
listener that reopens and re-sends the same XHR does so from *inside* the
terminating sequence, and releasing the root after `loadend` would silently
un-root a request that had only just started. That check is what makes the
common `x.onload = () => { x.open(...); x.send(); }` chain work.

### The event sequence

| Trigger | Events, in order |
| --- | --- |
| `send()` | `loadstart` (0, 0) |
| each `Chunk` | `readystatechange` (LOADING) + `progress` |
| `Done` | `readystatechange` (DONE) + `load` + `loadend` |
| error | `readystatechange` (DONE) + `error` + `loadend` |
| `abort()` | `readystatechange` (DONE) + `abort` + `loadend` |
| timeout | `readystatechange` (DONE) + `timeout` + `loadend` |

`load`/`error`/`abort`/`timeout` are mutually exclusive: exactly one fires, and
`loadend` always follows it.

`lengthComputable` is `true` **only** when the response carried a
`Content-Length` — absent for cached and chunked responses. `total` is 0
otherwise; it is never fabricated from the bytes that happen to have arrived,
because a progress bar computed from a fabricated total is worse than one that
knows it cannot be drawn.

That header is server-controlled, so it is parsed as the `unsigned long long`
the IDL says it is (`content_length_total`). Parsing it as `f64` accepted `NaN`,
`Infinity`, `-1` and `1e999` off the wire and then reported
`lengthComputable === true` for them — a fabricated total by another route, and
the worst kind, since `loaded / total` comes out `NaN`.

### `ProgressEvent` reuses the ADR-0023 payload slot

`ProgressEvent` inherits from `Event`, not `UIEvent` — so it lives in a new
`crates/idl/webidl/xhr.webidl`, not in `uievents.webidl`. Its three members
nonetheless go into `EventData::ui` as `UiKind::Progress`, because that slot is
what gives an event interface its **brand**: `ProgressEvent.prototype.loaded`
called on a plain `Event` fails on the payload shape, exactly as every
`MouseEvent` getter does, with no per-interface slab tag. The other `UiPayload`
fields (`detail`, `has_view`, `modifiers`) stay at their defaults and are never
read for it.

`loaded`/`total` are `unsigned long long` in the IDL and are stored as `f64`.
JS numbers *are* doubles, so below 2^53 — which is every transfer this engine
will see — the script-visible behavior is identical.

**`ProgressEvent` is deliberately absent from `document.createEvent`'s table.**
DOM's table is a closed legacy list, and `Document-createEvent.https.html`
asserts by name that every event interface outside it throws
`NOT_SUPPORTED_ERR` — `ProgressEvent` included. Adding it would have traded a
passing conformance test for a construction path `new ProgressEvent()` already
provides.

### The spec inheritance chain is declared in full

`EventTarget` ← `XMLHttpRequestEventTarget` ← {`XMLHttpRequest`,
`XMLHttpRequestUpload`}. The seven shared handlers (`onloadstart`, `onprogress`,
`onabort`, `onerror`, `onload`, `ontimeout`, `onloadend`) belong to the base, so
`xhr.upload.onprogress` and `xhr.onprogress` are the same member on two objects;
`onreadystatechange` stays on `XMLHttpRequest`.

They are typed `any` rather than `EventHandler` on purpose. An `EventHandler`
attribute also joins `EVENT_HANDLER_TYPES`, which is the list of event-handler
**content** attributes — and `<div ontimeout="…">` / `<div onreadystatechange="…">`
are not handlers in HTML. The accessors are hand-written over the same
`event_handlers` registry instead.

**`XMLHttpRequestUpload`'s slab entry holds a `Weak` back-reference** to its
owning `XhrData`. The `XhrData` holds the upload *wrapper* strongly — that is
what `[SameObject]` means — so a strong pointer the other way would close a
cycle: the wrapper would keep the JS object alive, the live JS object would keep
its slab entry, and the slab entry would keep the `XhrData` that holds the
wrapper. With a `Weak`, dropping the `XhrData` drops the wrapper and the upload
object becomes collectable; its listeners are then freed by the finalization
path ADR-0023 added.

### `this_xhr` returns the receiver alongside the state

`open()` cannot re-root a wrapper it was never handed. `BindCx::this_xhr` now
yields an `XhrRef` — the `Rc<RefCell<XhrData>>` plus the object the call arrived
on — which is also what `event.target` must hand back. Paths that reach an XHR
without a receiver (net delivery, the timeout timer) rehydrate an `XhrRef` from
the live self-root, which is set for exactly as long as those paths can run.

### Response header hygiene reuses `HeadersData`

`getResponseHeader` and `getAllResponseHeaders` filter `Set-Cookie` and
`Set-Cookie2`, then delegate to `HeadersData::get` and
`HeadersData::sorted_combined` — the same code the `Headers` interface uses to
sort by lowercased name and combine duplicates with `, `. One implementation of
those rules, two callers.

`setRequestHeader` gained the matching request-side hygiene: `InvalidStateError`
unless OPENED with the send() flag clear, a **silent** return for a forbidden
header name (the spec is explicit, and feature-detecting code sets `User-Agent`
and carries on), and **combining** a repeated name rather than pushing a
duplicate. The forbidden-name list moved to
`oxidepage_net::is_forbidden_request_header`, next to the net layer that also
strips those names off the wire.

### `responseText` decodes with the response charset

It was an unconditional lossy UTF-8 read, which mangled every non-UTF-8
response. It now uses the spec's **final charset**: the charset of an
`overrideMimeType()` value if it named one, else the response's own, else UTF-8.
`decode_charset`'s parameter parsing was factored into
`oxidepage_net::charset_from_content_type` so both callers ask the question the
same way.

### `responseType = "arraybuffer"` copies bytes directly

`JsScope::new_array_buffer(&[u8])` is a **new trait method** on the engine
backend (`ArrayBuffer::new_copy` on the QuickJS side), and it replaced the
bootstrap's `bytesToArrayBuffer` helper for all three callers
(`XMLHttpRequest.response`, `Response.arrayBuffer()`, `Request.arrayBuffer()`).

The route it replaced built one boxed `JsValue::Number` **per byte**, then a JS
array of that length, then `new Uint8Array(array).buffer` copied it again — tens
of megabytes of transient allocation and a visible pause for a 10 MB download,
over data already contiguous in Rust. The helper is gone from `bootstrap.js`
with it.

## The four deliberate limits

### No `Blob`

`responseType = "blob"` is unsupported. The engine has no `Blob` type at all —
`Response` omits `blob()` for the same reason
(`crates/idl/webidl/fetch.webidl`). Assigning `"blob"` leaves the previous
value, which is what an enumerated attribute does with a value outside its set
anyway, so `xhr.responseType = 'blob'` followed by a read tells the truth about
what the object will do. A mode that installed itself and then returned `null`
forever would be the P6 failure.

### No synchronous mode

`open(..., async = false)` throws `InvalidAccessError`. A blocking net wait
inside a JS call runs while the caller holds `RefCell` borrows on
`dom`/`style`/`layout`; the first thing the resumed page touched would be a
re-entrancy panic. Refusing loudly beats a mode that deadlocks the process.

### One `progress` event, at 100%

`crates/net` buffers the whole body and emits exactly one `Chunk` — an explicit
ADR-0004 decision, not an oversight here. So one `progress` fires per response
today.

**The XHR side is nonetheless written against a chunk *stream*:** `loaded`
accumulates per `Chunk` and a `progress` fires per `Chunk`. When the net layer
learns to stream, XHR needs no change. `crates/page/tests/network.rs` grew a
`/chunked/<n>` route that writes-sleeps-writes precisely so that loop is
exercised against a real chunked response rather than only against the buffered
path.

The upload side reports completion at 100% in one step, because the request body
is handed to hyper whole. Its `progress`/`load`/`loadend` fire when the response
head arrives — the earliest point at which the body demonstrably went out —
rather than synchronously inside `send()`, so `xhr.upload`'s `load` still
precedes the download's.

"100%" is the byte count `send()` recorded (`XhrData::upload_total`), reported as
`loaded == total` with `lengthComputable` true. Firing those three events with
`(0, None)` instead said the opposite of what the comment above them claimed,
and made every `e.loaded / e.total` bar read 0% — or `NaN` — at the exact moment
the upload finished.

### `timeout` is a page-side timer

`timeout` arms an event-loop timer plus `NetService::abort(id)`, not per-request
net plumbing. Observably this is correct: the sequence, the state and the
`responseText` reset are the spec's. The cost is that the socket may read on to
completion and then be discarded. Re-assigning `timeout` mid-flight re-arms
against the moment `send()` was called, which is where the spec measures from.

## Verification

**WPT's `xhr/` suite is not vendored, and this is a decision rather than an
oversight.** `xtask`'s `TestServer` (`xtask/src/testserver.rs`) cannot parse the
request method, read a request body, return a custom status or header, redirect,
chunk, or execute WPT's `.py` handlers and `.sub.` substitution — and `xhr/` is
almost entirely server-driven (`trickle.py`, `redirect.py`, `auth.py`). Making
it viable would be a larger project than XHR itself.

Verification is the loopback server in `crates/page/tests/network.rs`, which is
already far more capable: `route()` receives method, path, body and raw head,
and `resp()` takes a status, reason and extra headers. It gained a
parameterized `/delay/<ms>` (the previous delay was hardcoded to two paths) and
the `/chunked/<n>` route above.

Seventeen tests cover the event sequence and its order for success, error, abort
and timeout; `loadend` always last and the four terminal events being mutually
exclusive; `ProgressEvent` members and construction; `upload` events with and
without a body; `readyState` reaching 3; **a reused XHR firing events on its
second request**; `open()` terminating an in-flight request; the `send()` and
`setRequestHeader` state errors; the response reset on abort; every
`responseType` including the `responseText`/`responseXML` throws; `timeout`
firing and being cancellable; `overrideMimeType` changing the decoded charset;
`responseURL` after a redirect; `Set-Cookie` **not** being readable;
`getAllResponseHeaders` sorted and combined; and `setRequestHeader` combining
and ignoring forbidden names.

WPT went from 16309 to **16312** passing subtests with no regressions: making
`responseType = "document"` real turned three `custom-elements/upgrading.html`
subtests ("an HTML document fetched by XHR") from FAIL to PASS.

## Consequences

- A page can now drive XHR the way libraries actually do — reuse one object,
  read `upload.onprogress`, set `timeout`, ask for a parsed document — instead
  of hitting silence or a stale response.
- `Set-Cookie` is no longer script-readable off an XHR response. Any code that
  was (incorrectly) reading it will now see `null`.
- Two behavior changes are visible to existing callers: `open()` throws
  `SyntaxError` for an unresolvable URL where `send()` used to throw a
  `TypeError`, and `send()` before `open()` throws `InvalidStateError` rather
  than a `TypeError`. Both are the spec's; one in-repo test moved to an absolute
  URL because the bindings harness document is `about:blank`.
- `DomExceptionKind` gained `InvalidAccessError`.
- The upload object's `Weak` back-reference is the one place in the bindings
  where a slab entry does not own its data. It is documented at
  `BindCx::new_xhr_upload`, and the alternative is a leak with no path to
  collection.
- Real incremental download progress still waits on the net layer learning to
  stream (ADR-0004). The XHR half of that work is already done.
