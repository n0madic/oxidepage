# ADR-0032: Request interception, file inputs and downloads

- Status: accepted
- Date: 2026-08-03
- Builds on: ADR-0004 (the net↔page bridge and the one-park rule), ADR-0027 (browser, contexts, commands), ADR-0029 (`data:` subresources), ADR-0030 (CDP transport and the remote object model), ADR-0031 (the `Input` and `DOM` domains)
- Constrained by: design §2 (P6 "absent beats fake", P7 "conformance is automated"), §7 (threading), §8 (security)

## Context

ADR-0030 and ADR-0031 took `cargo xtask puppeteer` to 33 of 33, and the roadmap's
stage-8 milestone is "Puppeteer covers the 90% run end to end". Four capabilities
stood between the two, and all four were named in those ADRs' *deliberate limits*
as **scheduled, not absent**:

- `Fetch.enable` was refused, so `page.setRequestInterception(true)`,
  `request.respond()`, `request.abort()` and `page.authenticate()` all threw.
- `Blob`, `File`, `FileList` and `FileReader` did not exist; `input.files` was
  absent; `DOM.setFileInputFiles` answered `-32000` naming this stage; `FormData`
  entries were strings only, so a file input contributed nothing to a form post
  and clicking one did nothing at all.
- `<a download>` was skipped with a warning, `Browser.setDownloadBehavior('allow')`
  errored, and a `Content-Disposition: attachment` navigation was parsed as HTML.
- `Network.emulateNetworkConditions` was `method_not_found`; `crates/net` had no
  offline or latency concept at all.

A fifth thing was wrong and only visible once interception was designed:
`page.goto()` resolved to `null`. Puppeteer's `isNavigationRequest` is
`requestId === loaderId && type === 'Document'`, and the endpoint emitted neither
half.

## Decision

### D1 — The pause point is in `NetService`, above both fetch shapes, and never below HTTP

The gate goes at the top of `NetService::spawn_fetch` and of
`fetch_blocking`/`fetch_blocking_tracked` — after `next_request_id()` and
`note_request()`, before the request goes anywhere. This is ADR-0029's argument
reused: one funnel, so scripts of all four flavours, ES modules, `<link>` sheets,
`@import`, images, fonts, `fetch`/XHR **and the top-level document** get
interception with no per-consumer change. Doing it in `page` instead would miss
`fetch_blocking`'s five callers — including the one request a driver most wants,
`commit_document`.

Two properties make the *placement* load-bearing rather than incidental:

- **It must be before `pool.handle().spawn`.** The concurrency permit is acquired
  *inside* the spawned task, bounded by `MAX_CONCURRENT_FETCHES = 16`. A pause
  inside the fetch future would let sixteen paused requests starve every
  unpaused request on the page.
- **It must pause `http`/`https` only.** `fetch_inner` early-returns for `file://`
  and `data:` *above* the scheme gate (ADR-0029). Puppeteer's
  `NetworkManager.#onRequestWillBeSent` skips `data:` outright, so it never
  stores a `requestWillBeSent` for one; a `Fetch.requestPaused` for a `data:` URL
  is stored and **never continued**, hanging every inline image, font and module
  until the timeout. The predicate lives next to the ADR-0029 comment so the two
  cannot drift.

### D2 — Out on the existing observer; in on one channel the page also owns

- **Out.** `NetworkEvent` grows `Paused { id, request, resource_type }` and
  `AuthRequired { id, url, challenge }`. They ride the existing `NetObserver` →
  `PageRecord::Network` → `PageEvent::Network` → `pump.rs`. **No new event bus.**
- **The bus must not drop them.** `PageEvent::Network` is delivered with
  `try_send` and is silently dropped on a full bus. A dropped `Paused` wedges the
  page for the whole timeout — the same failure the dialog announcement already
  has a carve-out for. The `load_bearing` predicate therefore grows to cover
  `NetworkEvent::Paused` and `AuthRequired`, which routes them through
  `send_timeout` — **and `Requested` with them**, one step removed: a driver
  *pairs* `Fetch.requestPaused` with the `Network.requestWillBeSent` it already
  stored, and Puppeteer parks a pause whose partner never arrived to wait for
  it, forever, on a request it will therefore never continue. Letting only half
  the pair survive a full bus is the same wedge with an extra step.
- **In.** One **unbounded** crossbeam channel of `InterceptCommand`. It must be
  unbounded, not the `bounded(0)` rendezvous dialogs use: an async-paused page is
  not parked and may be mid-parse for seconds, so a rendezvous send would block
  the shared priority lane and violate D4's own bar.
- **The page owns a `Sender` clone.** `wait_for_work` documents that a
  *disconnected* `Receiver` is permanently ready in a `Select`, so a receiver
  whose only sender lives on the driver side converts the one park into a pegged
  core the moment the driver goes away. Same shape as `wake_tx`/`wake_rx`; the
  driver gets a clone.
- **Config.** `Arc<Mutex<InterceptConfig>>` shared between driver and page:
  patterns, `handle_auth`, the network conditions, and the live
  `paused: HashSet<RequestId>`. `Fetch.enable`/`disable` need no round trip, and
  idempotence lives on the CDP side — `continueRequest` removes the id from that
  set and only then sends, answering Chrome's `Invalid InterceptionId` otherwise.
  That is what makes two sessions with `Fetch` enabled safe: the first continue
  wins, and Puppeteer's `_continue` already catches the loser.

### D3 — Two resolution shapes, one channel

- **Async (`start_resource`).** On a match, do **not** spawn: park the
  `NetRequest` in `NetService::paused` and return the id immediately. Callers
  already store `pending_*` state under it and tolerate unbounded delay. The
  event loop gains `drain_intercept_decisions()` and `wait_for_work` a fourth
  `Select` arm. `Continue(overrides)` spawns under the **same** id; `Fulfill`
  synthesizes `NetEvent::Headers` + `Chunk` + `Done`; `Fail` emits
  `NetEvent::Error`. Every consumer already handles those four, so nothing
  downstream learns interception exists.
- **Blocking (`fetch_blocking`).** The page thread parks on the same receiver
  with `recv_deadline` — **not** `recv_timeout` in a loop, because a foreign-id
  decision would otherwise restart the clock and extend the park without bound —
  then acts inline: `Continue` → `block_on(engine.fetch(modified))`, `Fulfill` →
  return a synthesized `FetchOutcome`, `Fail` → `Err(NetError)`. Decisions for
  **other** ids buffer into a `deferred: RefCell<VecDeque<InterceptCommand>>`,
  drained at the top of the loop.

**The blocking park services nothing else — no `net_rx`, no `wake_rx`, no control
jobs.** `dispatch_net_event` enters JS, and two of the five `fetch_blocking`
callers park while holding live borrows: `PageCssFetcher::fetch_css` (called from
inside stylo's `@import` resolution with `dom` and `style` borrowed) and
`ModuleLoader::load` (called from inside QuickJS). Running script there is a
deterministic `BorrowMutError`. `wake_rx` is a level trigger so ignoring it loses
nothing, and honouring public `PageJob::control` where `dom` is borrowed would
turn a documented convention into a sharp undocumented requirement. This is
exactly what `run_dialog` already does.

The one-park rule (ADR-0004, ADR-0027 D4) holds: one more arm on the existing
`Select`, plus a separate blocking park that `run_dialog` already established as
legitimate.

### D4 — The `Fetch` resolution commands run on the `PRIORITY_LANE`

`is_priority` gains `Fetch.continueRequest`, `fulfillRequest`, `failRequest`,
`continueWithAuth` and `disable`. The bar stated there is "must interrupt work in
flight **and** cannot block itself"; with D2's unbounded channel these do a
mutex-guarded set removal and a non-blocking send, so they clear it.
`Fetch.disable` belongs because it releases every paused request.

**`Fetch.enable` does not** — it only writes shared config, and the driver's own
lane already orders it before the `Page.navigate` that follows. Without this, a
`Page.navigate` occupying the session lane while its own document fetch is paused
deadlocks against the command that would release it — verbatim the reasoning
`session.rs` already records for dialogs.

`fulfillRequest` base64-decodes on the shared lane, so the body is size-capped and
an oversized one is refused.

### D5 — Interception is not an SSRF bypass, and fulfilling is a deliberate CORS hole

The roadmap's binding constraint. A `continueRequest` URL override re-enters
`fetch_inner` from the top, so `scheme_allowed`, the per-hop re-check and the
connector's per-connect address filter all apply unchanged. Additionally:

- The override is **re-parsed and validated at the pause boundary**, so a
  malformed or non-`http(s)` override answers `invalid_params` on the command
  rather than failing the request minutes later with a confusing `NetError`.
- **`fulfillRequest` reports `ResponseType::Basic`.** That lets script read a
  cross-origin `no-cors` body it could never otherwise read. Chrome behaves the
  same way and `request.respond()` depends on it, so it is the right answer — but
  it is recorded as an explicit decision, because the driver is the operator and
  this is a deliberate hole, not an oversight.

### D6 — `NetRequest` carries a `ResourceType`, set per call site

`Document` / `Stylesheet` / `Image` / `Media` / `Font` / `Script` / `Xhr` /
`Fetch` / `Other` (CDP's spelling). It **cannot** be derived from the
constructor: `NetRequest::subresource` serves scripts, images, fonts *and*
stylesheets. Eight call sites set it as a field — two document, two stylesheet,
four script, one image, one font in `page`, plus the one site outside `page`
(`bindings`, where `fetch()` is `Fetch` and `XMLHttpRequest` is `Xhr`).

It must ride `NetworkEvent::Requested`, because Puppeteer reads `resourceType`
off **`Network.requestWillBeSent`**, not off `Fetch.requestPaused`.

### D6a — `loaderId`, so `page.goto()` stops returning `null`

Puppeteer's `isNavigationRequest` is `requestId === loaderId && type ===
'Document'`. The endpoint emitted `"loaderId": ""`, so it was always false, so
`LifecycleWatcher` never captured the navigation request and `page.goto()`
resolved to `null` — which the harness tolerated with `response === null ||`.

Emitting a real loader id, and minting the document request's protocol id
**equal to** it, closes a live bug that `type` alone does not. The document
request is recognised by `ResourceType::Document`, and both `requestWillBeSent`
and `responseReceived` must agree on the substituted id or `goto` hangs instead
of returning `null`.

**The loader is minted when the navigation *starts*, not when its request goes
out.** That is a correction the Puppeteer run forced, and it matters for a
second, independent reason: `Page.lifecycleEvent { name: "init" }` is the
**only** event that sets Puppeteer's `frame._loaderId`, and `LifecycleWatcher`
resolves a navigation only once that value has *changed* from the one it
captured beforehand. Minting at the request left `init` carrying the outgoing
loader, which was harmless only by luck — until a navigation *failed without
committing*, after which the committed loader had not moved either and the
**next** `goto` hung for its full timeout. Minting at
`NavigationEventKind::Started` is what Chrome does and makes all four consumers
agree: `init`, `requestWillBeSent`, `frameNavigated` and the post-commit
lifecycle events all name the same id.

So there are two loader ids on a target, and the distinction is load-bearing:
the **committed** one, which only a commit changes — a failed navigation must
not retire the current document's id, or a driver telling documents apart by
loader sees a phantom — and the **pending** one, which every event of the load
in flight carries. After the commit they are the same value.

### D7 — Release on disconnect is explicit, and the answer is `Continue`

Because the page owns a `Sender` (D2), the channel never disconnects, so there is
no automatic signal. Four paths must explicitly release every paused id:
`Connection` drop / socket close, `Target.detachFromTarget`, `Fetch.disable`, and
target destruction. The timeout is the backstop for a wedged driver holding the
socket open.

**The release semantics are `Continue` unmodified, not `Fail`** — what Chrome does
when the interceptor goes away, and the safe answer: failing would break a page
whose driver merely crashed.

**Releasing is two things, and the second is easy to miss.** Draining the paused
set without also clearing `enabled` leaves the page pausing every *subsequent*
request with nobody left to answer — so each one waits out the full timeout and
each announcement blocks the page on a bus no one drains. `release_all` is
therefore `disable()` plus the releases, not just the releases.

`DEFAULT_INTERCEPT_TIMEOUT` must be **strictly below** `DEFAULT_COMMAND_TIMEOUT`
(30 s) or a `Page.navigate` whose document pause goes unanswered reports
`EngineError::Timeout` to the driver *while the page is still loading*. It is
**20 s**, and the constant's doc names the constraint.

**The timeout applies to both halves, and only one gets it for free.** The
blocking half parks on `recv_deadline`; the asynchronous half is not waiting on
anything, so its deadline is stored on the pause and swept by
`drain_decisions` — which the event loop already runs on every pass. Without
that sweep an unanswered async pause produces **no terminal event at all**:
`in_flight` never returns to zero, the page is never idle, and every later
`settle` burns its whole budget. That is the same "zero terminal events" failure
ADR-0030 D5 records, reached from a new direction.

### D8 — Auth is Basic only, and the retry is a second pause, not a new mechanism

On a 401 or 407 carrying `WWW-Authenticate: Basic` (or `Proxy-Authenticate`),
with `handleAuthRequests` on: the outcome is **stashed rather than delivered**,
`NetworkEvent::AuthRequired` is announced, and the request re-enters the same
`paused` map. `continueWithAuth` with `ProvideCredentials` re-issues under the
same id; `Default` and `CancelAuth` emit the stashed outcome through the same
`emit_outcome` helper. Async carries the stashed `FetchOutcome` back on an
internal `AuthPause` record; blocking already has it inline. **No tokio
synchronisation primitive and no second rendezvous** — it reuses `paused`,
`recv_deadline` and `emit_outcome`.

Two details are load-bearing and neither is obvious:

- **The credentials go in the header the *challenge* named.** A 407 answered
  into `Authorization` instead of `Proxy-Authorization` is refused by every
  proxy, and the request then re-challenges forever — so the pause keeps the
  `AuthSource`, it does not merely announce it.
- **They travel on `NetRequest::auth`, not in `headers`.** Both auth header
  names are Fetch *forbidden request headers*, so anything placed in `headers`
  is stripped before it reaches the wire. That rule governs what **script** may
  set; it must not stop the user agent from answering a challenge it was asked
  to answer. A single-purpose field rather than a general "trusted headers"
  escape hatch, because a general one would be a way to smuggle any header at
  all onto a cross-origin `no-cors` load. It is dropped on a cross-origin
  redirect, exactly as the script-supplied credential headers beside it are.

**The retry is capped at one.** A server that refuses the credentials
re-challenges, so a driver answering `ProvideCredentials` unconditionally would
loop — one request per round trip, and no terminal event ever. Past the cap the
stashed 401/407 goes through to the page, which is what a browser shows when a
user gives up. The blocking half was already straight-line; the async half needs
the count stored, because its retry goes back out through the same pause map.

Digest, NTLM and Negotiate are refused by name, not silently downgraded. There is
no credential cache: a challenge is answered per request, as CDP defines it.
`Fetch.enable { handleAuthRequests: true }` is **accepted regardless** of whether
any auth is ever seen, since Puppeteer sends it unconditionally in
`#applyProtocolRequestInterception`.

### D9 — `offline` and `latency` are real; bandwidth shaping is refused

`ResourcePolicy` is browser-wide (ADR-0027 D8) and `emulateNetworkConditions` is
per-session, so this is **page-level state on `NetService`**, checked at the same
pre-dispatch hook the pause uses. `offline` fails every request with
`net::ERR_INTERNET_DISCONNECTED`; `latency` sleeps **outside**
`FetchEngine::fetch`'s `timeout(request_timeout)` wrapper so it does not eat the
request budget.

`downloadThroughput`, `uploadThroughput` and `connectionType` are refused with a
message naming why (ADR-0030 D9's rule): approximating bandwidth without a token
bucket is the silent half-truth P6 forbids, and the roadmap already says to
reject it explicitly. A throughput of `-1` (Chrome's "no limit") is accepted,
because that asks for nothing.

### D10 — One `BlobData`, two interfaces

`HostData::Blob(Rc<BlobData>)` with `BlobData { bytes: Rc<Vec<u8>>, type_:
String, start, end, file: Option<FileMeta> }`, so `slice` is a view, not a copy.
`interface File : Blob` (IDL inheritance is supported); `this_blob` accepts both
and `this_file` demands the file metadata — the shape `this_xhr_event_target`
already uses.

Codegen constraints force two hand-marshalled signatures: `sequence<T>` as an
**argument** and any typed-array or `ArrayBuffer` type are build-time errors, so
`new Blob(parts, options)` and `formData.append(name, value, filename)` take `any`
and unmarshal in `imp/`. `sequence<T>` as a *return* is fine.

`FileList` is the `DOMRectList` shape and **must** join `install_value_iterators`
— ADR-0031 D6's lesson: an indexed-getter interface absent from that list makes
`[...el.files]` throw for no reason a page author can see.

`FileReader` read completion is queued as a **task**, not resolved inline, so
`onloadend` observes the same ordering a browser gives it.

### D11 — Selected files live in `dom::FormState`, as plain data

`FormState` gains the selected-file list beside the dirty-value and checkedness
flags. It is a **`dom`** type — `dom` cannot see `bindings`, and files enter only
from the embedder (`DOM.setFileInputFiles`, the chooser), never from page script:
there is no `DataTransfer`, so `input.files` is read-only in practice. `bindings`
wraps it into a `File` on read. Mutation goes through a `DomTree` primitive so the
one invalidation code path is preserved, and the list is reset on a `type` change
and on form reset.

`Page::set_file_input_files` reads via `std::fs` and **not** through
`net::file::load_file`, which is gated on `ResourcePolicy::allow_file` — off by
default, and about *page-initiated* loads. A driver setting file inputs is the
operator; conflating the two either breaks the command or silently widens the
page's own `file://` reach.

### D12 — The file chooser is embedder-mediated and does not park the page

`<input type=file>` activation gains an `Activation::FileChooser` variant. With no
interception installed it does nothing — the honest headless answer, the shape
ADR-0025 chose for `alert`. With `Page.setInterceptFileChooserDialog(true)` it
emits `Page.fileChooserOpened` and the driver answers with
`DOM.setFileInputFiles`. Unlike a modal dialog it does **not** park the page
thread: a chooser has no return value the activation needs.

The announcement is a **task source**, not an emit from the click's own stack,
for one concrete reason: the event has to carry a `backendNodeId`, and the handle
table lives on `Page` rather than on the hooks the activation runs through. That
field is not optional in practice, whatever the protocol says — Puppeteer's
`#onFileChooser` calls `adoptBackendNode(event.backendNodeId)` immediately, so an
event without one leaves `page.waitForFileChooser()` hanging until its own
timeout. (This ADR first claimed the field was unread; the Puppeteer run
disproved it, which is what the acceptance gate is for.)

### D13 — A download is a navigation that does not commit

`commit_document` learns to read `Content-Disposition` (no parser existed — the
only occurrence in the tree was the multipart *writer*). When it names
`attachment` and the behavior is `allow`, the bytes go to the download directory,
`downloadWillBegin` and `downloadProgress` are recorded, and the current document
**stays** — what a browser does. With `deny` (the default) the navigation is
refused and recorded, not parsed as HTML. `<a download>` routes through the same
path instead of warning and skipping.

The attribute is a download request in its **own right**, not a hint the
response may veto. Requiring `Content-Disposition: attachment` as well is the
reading that fails in the exact case the attribute exists for: a static file
server answers `/report.pdf` with the bytes and no disposition header at all, so
the click committed the response as a document and replaced the live page with a
PDF read by the HTML parser. Honouring it is not the page overruling the
operator either — the attribute decides *that this is a download*, and
`DownloadBehavior` still decides whether anything is written, denying by
default. The attribute's value is the suggested filename, sanitized like every
other attacker-influenced name.

Same-origin only, as in Chrome: a cross-origin `download` is ignored and the
link navigates, so a page cannot make another site's response land on disk under
a name of its choosing.

`oxidepage serve` grows `--download-path <dir>`; no directory *is* deny, which is
what `Browser.setDownloadBehavior` already said. The filename is derived from
`Content-Disposition` with path separators stripped, a traversing `downloadPath`
is refused, and an existing file is not overwritten — a suffix is appended.

## Verification

**`crates/net/tests/intercept.rs`** covers the funnel against a loopback server:
exactly one terminal event per fulfilled id (the contract ADR-0030 D5 says has
already cost a driver twice), abort-while-paused, a `data:` URL never pausing,
continue-with-overrides, fulfil, fail, a 401 Basic challenge answered with
`ProvideCredentials`, and `offline`. **`crates/page/tests/intercept.rs`** covers
the page side: navigation-while-paused, a blocking `@import` pause taken under
live `dom`/`style` borrows (the `BorrowMutError` regression), and `loop_stats`
showing the park count unchanged per iteration (ADR-0004's one-park criterion).

**`crates/cdp/tests/fetch.rs`** drives the domain over a real socket, including
both explicit release paths — a session detaching and a socket closing while a
navigation is paused, each driven from a *second* connection because the first
is blocked inside its own `Page.navigate`.
**`crates/page/tests/navigation.rs`** pins the multipart wire format with a file
part; that an *empty* file input still contributes an empty part when the form
is genuinely multipart; and that it does **not** upgrade the enctype of a form
that declared none, since there are no bytes for urlencoded to lose.
**`crates/page/tests/downloads.rs`** pins that an attachment does not commit,
that a traversing filename cannot escape the directory, that an existing file is
never overwritten, that `<a download>` downloads a response carrying no
`Content-Disposition` while a cross-origin one navigates instead, and that two
pages never mint the same download guid. **`crates/cdp/tests/dom.rs`**'s `setFileInputFiles`
refusal assertion flips to a success assertion, plus the chooser being silent
until intercepted. **`crates/cdp/tests/page.rs`** pins D6a's two loader ids:
each navigation announces a fresh loader on `init`, and a failed one does not
commit an id.

**`cargo xtask puppeteer`** is the acceptance gate: **45 of 45**, with
`tests/automation/expectations.tsv` holding no entries. The twelve new checks
cover `setRequestInterception` (continue, respond, abort, URL override),
`page.authenticate`, offline emulation, `elementHandle.uploadFile`,
`page.waitForFileChooser`, a multipart upload asserted on the wire, a download,
`Blob` + `FileReader`, and `response.text()` of the navigation itself. Two of
them found bugs this ADR had reasoned wrongly about — the `init` loader (D6a)
and the chooser's `backendNodeId` (D12) — which is what the gate is for.

`xtask`'s test server grew three **behavioural routes** under a reserved `/-/`
prefix to support them: it served static files and never read a request body, so
it could express neither an upload target, nor an attachment, nor a 401
challenge.

## Corrections found in review (2026-08-04)

Ten defects in the decisions above, found by review of the landed commit and
fixed together. Each was a rule this ADR states and the code then broke, so they
are recorded here rather than in a new ADR:

- **D7's release raced its own lanes.** `release_all_interception` runs the
  moment the read loop ends, but lane threads keep draining queued commands
  until their senders drop — so a `Fetch.enable` behind a slow `Page.navigate`
  landed *after* the release and re-armed interception with nobody left to
  answer. Every later request on that page then paused for the full timeout,
  permanently, and `serve` hands that page to the next driver.
  `Connection::closed` gates the job.
- **D8's retry named the wrong URL.** The retry snapshot is taken before
  `FetchEngine::fetch` follows redirects, while the challenge comes from
  `final_url` — so credentials went on the *first* hop's URL and
  `strip_auth_on_cross_origin` then removed them. The challenging server never
  saw them however correct they were.
- **D6's overrides were filtered as if they were script.** `continueRequest`
  headers went into the script slot, which `no-cors` — every subresource —
  filters by the CORS safelist. They worked on documents and silently vanished
  on `<img>`/`<script>`/`<link>`. They now have their own slot beside `auth`,
  for the same reason `auth` has one, and win over the engine's own
  `user-agent`/`referer`/`origin`/`cookie`. The *framing* headers are still
  refused: a driver-set `Content-Length` is request smuggling, not automation.
- **D2's load-bearing set was half a bracket.** `Requested` was load-bearing and
  `Finished`/`Failed` were droppable, so a slow driver could get an opening with
  no closing and `networkidle0` would never resolve. Either both halves survive
  a full bus or neither does.
- **The `Fetch` domain is per session; the config is per page.** One session's
  `Fetch.disable` turned interception off for every other session attached to
  the target, leaving their flags `true` and no way to notice.
  `InterceptConfig::wanted_by` ends interception when the last session lets go.
- **`Fetch.disable` on the priority lane could overtake `Fetch.enable`** on a
  session lane, so an unawaited `setRequestInterception(true)` then `(false)`
  left interception on. Requests now carry their receive order and the config
  applies the driver's, not the lanes'.
- **Offline was checked above the pause point**, so `setOfflineMode(true)` +
  `request.respond()` — the standard offline-page test — failed every request
  including the ones the driver meant to stub. The gate moved to
  `spawn_fetch_with`, the one place a request actually leaves for the network.
- **D9's conditions were never cleared.** A driver that went offline and then
  dropped its socket left the page permanently offline for the next one.
- **A fulfilled `302` was delivered as a bodyless 302** rather than followed —
  for a document, a blank page with no error. `deliver_fulfilled` follows it,
  capped, and applies the same method rewrite a real redirect does.
- **The intercept timeout was measured per pause, not per operation**, so one
  request parking twice (request pause, then auth pause) exceeded the engine's
  command timeout and reported a navigation as timed out while it was still
  loading.

One more, unrelated to interception: `ContextOptions::merge` filled
`download_path` from the context's *construction-time* option, so the "live path
wins" line below it could never fire and
`Browser.setDownloadBehavior({behavior:"deny"})` was a silent no-op for every
page created afterwards. The per-page option is now read before the merge, and a
page that is alive but too busy to answer is reported rather than swallowed.

## Deliberate limits (P6 — absent beats fake)

- **`Fetch.continueResponse` and response-side body rewriting are absent** (a
  roadmap non-goal). `Fetch.enable { patterns: [{ requestStage: "Response" }] }`
  is **refused**, not silently downgraded to the Request stage, and
  `Fetch.getResponseBody` and `Fetch.takeResponseBodyAsStream` are refused for
  the same reason: there is no response-stage pause for them to read from.
- **`Network.getRequestPostData` is refused and `hasPostData` is never set
  true.** The stack retains response bodies (`RequestLog`) but not request
  bodies.
- **`Fetch.enable` on a page with no event sink is refused.**
  `set_event_sink(None)` removes the net observer, so nothing could announce a
  pause and the page would wedge for the whole timeout.
- **Interception is per *target*, not per session, on the teardown side too.**
  Two sessions attached to one target share one `InterceptControl`, which is
  what makes resolution idempotent (D2) — but it also means one session's
  `Fetch.disable`, a socket closing, or a `Target.detachFromTarget` turns
  interception off for the *other* session as well. That session's `flags.fetch`
  stays true and it simply stops receiving `requestPaused`, with no error. Two
  sessions intercepting one target is rare (a driver holds one), and
  refcounting enabled sessions across connections is more machinery than the
  case warrants — so it is recorded rather than fixed.
- **Digest, NTLM and Negotiate auth are refused by name**, and there is no
  credential cache.
- **No bandwidth shaping.** `downloadThroughput` and `uploadThroughput` (other
  than `-1`) and `connectionType` are refused.
- **No HAR recording, no WebSocket interception** (there is no `WebSocket` in the
  engine), **no service workers.**
- **No `URL.createObjectURL` and no `blob:` scheme.** A new scheme would widen
  the policy gate and nothing in the 90% run needs it.
- **No `DataTransfer`**, so `input.files` is settable only by the embedder and
  `setInputFiles` on a non-`<input>` target still fails (ADR-0031's limit holds).
- **`<input type=file>` has no default rendering.** There is no picker widget
  and no UA-stylesheet size for one, so an unstyled file input lays out 0×0 and
  cannot be clicked *by coordinate* — `page.click()` reports "not clickable".
  `element.click()` and a click on a styled input both work, and both open the
  chooser. Rendering a file-picker control is a layout concern, not an
  automation one, and inventing a size for a widget that does not exist is the
  fake P6 forbids.
- **`FileReader` has no `readAsBinaryString`** and no progress events beyond
  `loadstart`/`load`/`loadend`; `abort()` is honoured but reports no partial
  result.
- **A download reports no `guid`-keyed resume**, and `Browser.cancelDownload` is
  absent: a download is written whole or not at all, because the fetch that
  produced it was already read to completion in memory.
- **Response timing phases remain absent** (ADR-0030's limit is unchanged), and
  `Network.requestWillBeSent` still carries `initiator: { type: "other" }`.
