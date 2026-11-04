# ADR-0042: `blob:` URLs and `URL.createObjectURL`

- Status: accepted
- Date: 2026-11-04

## Context

`URL.createObjectURL(blob)` turns script-built bytes into a URL any loader
accepts. It is how a page shows a preview of a file the user picked, saves a
generated download through `<a download href=blob:…>`, runs code it
assembled (`<script src=blob:…>`, workers), or frames HTML it built. The
engine had no blob URLs and no `blob:` scheme.

That is a security question as much as a feature, for three reasons:

- A new scheme is a new way into the fetch pipeline.
- The bytes are attacker-controlled and live as long as someone holds the URL.
- A document loaded from a blob URL takes the origin of the page that made it.

ADR-0029 settled the analogous `data:` question. `data:` is answered above the
scheme gate, outside the redirect loop, so every consumer gets it and an
`http:` → `data:` redirect stays a network error.

## Decision

1. **One blob URL store per page, in `net`.** `net::BlobUrlStore` maps a URL,
   without its fragment, to a copy of the blob's bytes, its type and the frame
   whose document created it. It hangs off the page's `FetchEngine`, beside
   the per-page budget counters, and is shared by that engine's clones.

   The File API resolves a blob URL within the creator's *storage partition*.
   A page, its top-level document and every frame in it, is that partition
   here:
   - any frame of the page can resolve the page's blob URLs;
   - another page cannot, even one of the same site, and gets a network error.

   The bytes are copied once, at `createObjectURL`. A `Blob` is an immutable
   page-thread `Rc`, and the store is read from the network threads.

2. **`blob:` is answered above the scheme gate, exactly like `data:`.**
   `fetch_inner` returns early for `blob:` beside `file://` and `data:`:
   - A registered URL fetched with `GET` gets a `200` with `Content-Type` and
     `Content-Length`.
   - Any other method, or an unknown or revoked URL, is a network error, as
     Fetch's scheme fetch requires.
   - `max_response_bytes` applies, as it does to `data:`.

   `blob` is **not** added to `ResourcePolicy::allowed_schemes`. The redirect
   loop re-checks that gate per hop, so a redirect to even a registered blob
   URL is blocked. A hardening test pins this. The interception, offline and
   request-pause gates are `http`/`https`-only already, so blob loads never
   pause or fail offline. Chrome behaves the same, and a pause the driver
   cannot continue would hang the page.

3. **URLs and their lifetime follow the File API.**
   - `createObjectURL` mints `blob:<origin>/<uuid v4>`, from `getrandom`,
     under the calling realm's document origin. It refuses anything that is
     not a `Blob`: there is no `MediaSource`.
   - `revokeObjectURL` ignores anything that is not a `blob:` URL of the
     caller's own origin, so no page revokes what it did not create.
   - Every URL a frame's document created is revoked when that document is
     replaced (a navigation commit, after the new document was fetched, so
     navigating *to* one still resolves it) or when the frame is discarded.
   - Both statics are WebIDL static operations, which the codegen does not
     emit. The bootstrap defines them on the generated `URL` constructor over
     two captured native helpers.

4. **A blob document has its blob's origin.** The frame-origin helpers
   (`same_origin`, `origin_of`) look through a `blob:` URL to the URL it was
   minted under, as URL's origin algorithm does. So `<iframe src=blob:…>` is
   same-origin with the page that made it, and `MessageEvent.origin` names
   that origin.

5. **The store is bounded.** It holds at most `max_total_bytes`, the page's
   whole-response budget. A `createObjectURL` that would pass that throws
   `QuotaExceededError` instead of growing the page without limit.

## Consequences

- Blob URLs work for every consumer the fetch pipeline serves: `fetch`, XHR,
  classic and module scripts, style sheets, images, fonts, frame
  navigations and downloads.
- **Not shared across pages.** A blob URL handed to another page, even of
  the same site, does not resolve there. Browsers share the partition per
  top-level site; here a page is the partition. Pages of one browser context
  could share a store later without changing the scheme handling.
- **Within a page, a blob URL is a capability.** Any frame of the page that
  knows the URL can load it, whatever its origin, which is the File API's
  same-partition rule. Guessing one means guessing 122 random bits.
- Memory: an unrevoked URL keeps its copy until its creator's document
  unloads, bounded by the cap above. A revoked URL frees its bytes at once.
  That includes the case where a load is still in flight, because it holds its
  own `Bytes` handle.
- `data:` and `blob:` now share one rule: answered above the gate and never
  through a redirect. A new early return belongs beside them, not in the
  allowlist.
