# ADR-0029: `data:` URLs are decoded in the fetch pipeline, above the scheme gate

- Status: accepted
- Date: 2026-07-28

## Context

`ResourcePolicy::allowed_schemes` is `["http", "https"]`, and `file://` is
handled by an early return in `fetch_inner` before that gate. Everything else —
including `data:` — fell through to the gate and failed with
`blocked: scheme `data` is not allowed`.

That made `<script src="data:text/javascript,…">`, `<link rel=stylesheet
href="data:text/css,…">`, `@import url(data:…)` and `import` of a `data:` module
all fail. Acid3 exercises exactly this, in five spellings, and reported five
non-fatal `[script error]` lines while otherwise rendering.

`data:` was not absent from the engine: `page` had its own `decode_data_url`,
used inline by the `<img>`/background-image and `@font-face` paths, which decode
before touching the network so the bytes are available to layout in the same
turn. That decoder had two defects. It percent-decoded the payload only in the
*non*-base64 branch, while the Fetch data: URL processor percent-decodes first
and base64-decodes second — so any body whose base64 arrived percent-encoded
(`Url`'s serializer encodes `=` as `%3D`, and three of the five Acid3 cases are
encoded further) failed to decode. It also stripped MIME *parameters*, discarding
`charset=`, which a text subresource needs and an image does not.

Two placements were available. Decoding inline at each `page` call site would
have mirrored the image path and left `net` untouched, but there are six such
sites, and two of them are asynchronous: `<script async>`/dynamic scripts and
`<link rel=stylesheet>` complete through `NetEvent` and fire `load`/`error` from
the event loop. Decoding those inline would either run an `async` script
synchronously inside the parse loop — blocking the parser, which HTML forbids —
or require a new queue, a new task source in the documented drain order, and new
`in_flight` accounting. The alternative was one early return in `fetch_inner`.

## Decision

`data:` is decoded in `crates/net/src/data.rs` and dispatched from `fetch_inner`
by an early return beside `file://`, above the scheme gate. `data_outcome` wraps
the result in an ordinary 200 `FetchOutcome` whose single header is the declared
MIME type (parameters included, so `charset=` survives), which makes a `data:`
body indistinguishable from a fetched one to every consumer: classic scripts in
all four flavours, modules, stylesheets, `@import`, images, fonts, `fetch` and
XHR. Asynchronous consumers keep their ordinary `NetEvent` timing.

`allowed_schemes` is **not** widened. Placement above the gate but *outside* the
redirect loop is the load-bearing detail: that loop re-checks `scheme_allowed`
per hop, so an `http:` response redirecting to `data:` remains a network error,
as Fetch requires.

The budget counters are not charged, matching `file://`, which also returns above
them. There is no request to rate-limit and no body to stream, and
`max_response_bytes` guards a decompression bomb arriving over the wire, not
bytes the caller already held in memory.

`net::data::decode` — the processor operating on everything after `data:` — is
public, and `page`'s image and `@font-face` paths call it instead of the deleted
`decode_data_url`/`base64_decode`. Those two paths keep decoding inline, so they
are unchanged in timing; they now share one decoder with the pipeline, so an
inline decode and one that went over `net` agree byte for byte.

## Consequences

The decoder follows the Fetch data: URL processor and the Infra forgiving-base64
decode, so percent-encoded base64, embedded ASCII whitespace, optional padding,
a case-insensitive `;base64` marker, a leading `;` gaining `text/plain`, and the
`text/plain;charset=US-ASCII` default are all handled in one place. The
percent-decode-before-base64 fix reaches images and web fonts too, which
previously rejected any `data:` image or font whose base64 arrived encoded.

`fetch()` and `XMLHttpRequest` gain `data:` support as a byproduct. That is what
Fetch specifies, and it was not separately requested; it follows from putting the
decode in the pipeline rather than at the call sites.

The security posture is unchanged in substance: a `data:` URL carries its own
bytes, so there is no address for the SSRF filter to vet, no cookie to attach and
no CORS gate to apply, and the early return reaches none of them. The one thing
that could have changed — reaching `data:` through a redirect — is explicitly
pinned by a test.

`crates/net` gains a `percent-encoding` dependency (already a workspace dep, used
by `page`).

WPT moves by eleven lines, all in `css/cssom/HTMLLinkElement-disabled-00*`,
which link their stylesheets as `data:text/css,…` and so had never loaded one.
Three subtests newly pass and five files' harnesses go `TIMEOUT` → `OK`. Three
subtests now `FAIL`, one of them (`-001`, "`<link disabled>` prevents the
stylesheet from being in `document.styleSheets`") having previously *passed
vacuously*: `document.styleSheets` was empty because the sheet never arrived,
not because `disabled` was honoured. It is not — `start_link_stylesheet` reads
only `href` and `media`, and HTML's "explicitly enabled" state machine is
unimplemented. That gap is pre-existing and out of scope here; it is now visible
in `expectations.tsv` instead of hidden behind a load that never happened, which
is the point of the two-sided contract.

Coverage: `crates/net/src/data.rs` unit-tests the processor, three tests in
`crates/net/tests/hardening.rs` cover the response shape, the malformed case and
the still-blocked redirect, and `crates/page/tests/data_urls.rs` covers the
subresource kinds end to end, including the five Acid3 spellings verbatim and the
"an `async` script does not block the parser" property.
