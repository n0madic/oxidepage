# ADR-0034: Deferred command completion, the script-created parser, and the Playwright surface

- Status: accepted
- Date: 2026-08-08

## Context

Stage 9 (ADR-0033) brought real isolated worlds and with them a Playwright
suite that passed 13 of 17 checks. `docs/automation-roadmap.md` explained the
four failures with one phrase — they "need Playwright's injected-script
plumbing that stage 10's frame work brings" — and that explanation was wrong
about all four. Reading `playwright-core@1.54.1`'s own sources against the
engine gave four independent causes, of which exactly one was architectural:

- **`page.fill` appended instead of replacing.** Not the `Input` domain: it
  honours a selection, which is why Puppeteer's `page.type` works. Playwright's
  `injectedScript.fill` runs `input.select(); input.focus();` — selection
  *first* — and `move_focus` parked a collapsed caret at the end of the value
  unconditionally, erasing it before the paste arrived.
- **`page.exposeBinding` deadlocked.** A session lane is serial (ADR-0030), and
  `Runtime.callFunctionOn { awaitPromise: true }` blocked it for up to ten
  seconds. The promise it awaited could only be resolved by
  `deliverBindingResult` — a second command **on the same session**, which then
  sat behind the very call it had to release. After the budget the pending
  promise was serialized as `{}`.
- **`page.setContent` failed at the first statement.** `document.open` and
  `document.close` were not installed at all, so the whole evaluate threw a
  `TypeError`.
- **`page.waitForSelector` was a bug in our own harness.** It appended an
  unstyled empty `<div>`; `waitForSelector` defaults to `state: 'visible'` and
  Playwright's `isElementVisible` requires a non-empty box, so the server-side
  retry loop ran to its timeout. It would have failed against real Chrome too.

Implementing the first three surfaced three more defects that were latent
rather than new, and the stage is mostly the story of those.

## Decision

### D1. `awaitPromise` answers later instead of holding the session lane

An evaluation whose promise has not settled parks it and returns
`EvaluateOutcome::Deferred(token)`. The CDP lane sends **no** response and
moves on to the next command; the page answers from a task source of its own,
`PageEvent::AwaitSettled`, and the pump completes the request. Only
`Runtime.evaluate`, `Runtime.callFunctionOn` and `Runtime.awaitPromise` can
defer, and only under `EvaluateOptions::defer_await`, which the library API and
the CLI leave off — so `Page::eval` keeps blocking exactly as before.

Freeing only the lane would not have been enough. A nested job stops at the
page-level `in_job` guard, and removing that guard is the `BorrowMutError` it
exists to prevent (CLAUDE.md, "the command port is a task source"). The command
has to *return*, with the answer following separately.

Four properties are load-bearing rather than incidental:

- A promise that has **already** settled is still answered on the spot, so a
  driver only ever sees a token for a genuinely asynchronous wait and
  Puppeteer's paths are untouched.
- The token map is two-sided under one lock. The page can settle and emit
  before the lane learns it deferred, so an answer with no waiter parks rather
  than being dropped, and the lane checks for a parked answer before parking
  itself. Unclaimed answers are swept by age — the event bus fans out to every
  connection attached to the target, so a second driver sees tokens it will
  never claim.
- Tokens come from a process-wide counter, because one connection spans pages.
- **Every exit answers**: budget expiry, the navigation that destroyed the
  context, and the page's own close. Silence strands the driver, and the parked
  `JsValue` would outlive its `Runtime` — an abort inside `JS_FreeRuntime`, not
  a failing assertion. The `pending_awaits` field therefore sits above `worlds`
  in `Page`, and `Drop` clears it first.

### D2. `document.open`/`write`/`close`, and a replacement keeps its contexts

`document.open()` opens a **script-created parser** — a buffer that
`write`/`writeln` append to — and `close()` hands it to the page as a
`PendingNavigation::ReplaceDocument`, committed at the task boundary. It cannot
be synchronous: `close()` runs inside JS holding `RefCell` borrows on the DOM,
style and layout, which is the same constraint ADR-0022 met for script
navigation. A task that opens and writes but never closes still commits, at the
task boundary, because that is the legacy idiom and a browser shows that
content.

This reverses an explicit non-goal of ADR-0017/ADR-0022. What is *not*
implemented, deliberately:

- The buffer is not parsed incrementally between `write`s, and scripts in it do
  not run until `close()`. Invisible to `setContent`; a deviation for a page
  that opens a document and expects to see it grow.
- `document.write` **without** `open()` keeps the path it always had —
  warn-and-noop outside an active parser. The spec's implicit-open is not
  implemented, so the existing behaviour is unchanged rather than quietly
  widened.
- `open()` is refused on a document with no browsing context, which keeps
  ADR-0017's rule intact.

**A replacement preserves its execution contexts.** HTML's `document.open()`
reuses the `Document`, the `Window` and the environment settings object, so the
realms, their ids, and the handles minted against them all survive: the main
world is not renumbered, isolated worlds are not rebuilt, and the object stores
are kept. This is a deliberate, narrow carve-out from ADR-0033 D9, and it is
not cosmetic — a driver told its contexts died rejects every command it has in
flight against them, including the `callFunctionOn` that ran `document.close()`
in the first place. `Page::load_html` is the other caller and keeps the full
rebuild: an embedder replacing the content is a navigation in every way that
matters.

Committing the replacement through the ordinary path also closed a hole it did
not open: `Page::load_html` recorded neither `Started` nor `Committed`, so an
embedder's `set_content` produced no `init`, `frameNavigated` or `load` at all.

### D3. `Page::suspend` freezes the page, not the protocol

A suspended page runs none of its **own** sources — no timers, no rendering
opportunities, no queued navigation, no subresource load — while embedder jobs
and net events keep being served.

Note what that does *not* say: a suspended page is not script-free. A net event
delivered while suspended resolves the `fetch`/XHR promise waiting on it and
runs a microtask checkpoint, so page script does run — and an evaluate from the
driver runs page script by definition. The line is between the page's own
*scheduling* and the driver's turn, not between script and no script.

That is what `Target.setAutoAttach { waitForDebuggerOnStart: true }` means: the
page is stopped so it can be inspected and configured *before* it starts. Under
the old semantics it could not be honoured at all — suspension deferred every
ordinary job, and a driver sends its whole session setup
(`Page.addScriptToEvaluateOnNewDocument` among it) before
`Runtime.runIfWaitingForDebugger`, so pausing deadlocked the setup instead of
delaying the page. `Target.createTarget` now honours the flag, and
`waitingForDebugger` is read from `Page::is_suspended` rather than from the
request that asked for it — a target attached after it started is not waiting
for anything.

### D4. Focus no longer erases an explicit selection

`FormState` records whether the selection was asked for **by name**
(`select()`, `setSelectionRange()`) or merely left behind by an edit. Focus
collapses the caret to the end of the value only in the second case.
`set_form_value` closes the flag's life cycle by doing what HTML's value setter
requires anyway: a value write moves the cursor to the end and unselects.

### D5. A `Frame` type, and `Page.frameAttached`/`frameDetached` are not implemented

A frame's state — its id, its URL, its committed loader and the loader of the
load in flight — moved out of `TargetEntry` into `crates/cdp/src/frame.rs`,
with `frame_json` as a method on it. No behaviour changed. It is a landing
place: today a target *is* one frame, so "the loader of this target" and "the
loader of this frame" name the same string, and stage 11 makes `loader_id`
per-frame. Keeping the whole of a frame's state in one type makes that a change
of ownership rather than a rewrite of the loader bookkeeping ADR-0032 D6a
pinned down.

A deviation from the roadmap's text for stage 10, and the ADR wins
(`docs/README.md`). Chrome does not send either for the main frame, and
Playwright takes the main frame from `Page.getFrameTree` — `_handleFrameTree`
calls `_onFrameAttached` itself. With no nested browsing contexts there is
nothing else to attach, so both events would be dead code that no driver reads:
exactly the "fake" P6 forbids. They arrive with real iframes in stage 11, where
they will have something to describe.

### D6. `Emulation.setLocaleOverride`, both halves or nothing

Implemented through one narrow seam, `Page::set_languages`, which validates and
then writes `navigator.language`/`languages` **and** the `Accept-Language`
header. Moving one alone is precisely the dishonesty
`Emulation.setUserAgentOverride` is refused for, reached by a different road: a
page rendering in the wrong language while `navigator.language` insists
otherwise. The seam is not a general per-page header API —
`Network.setExtraHTTPHeaders` stays refused, and widening this is how that
refusal would stop meaning anything. An absent locale is refused rather than
silently ignored: there is no "locale before the override" to restore.

`Page.setBypassCSP` is accepted, because no CSP is enforced anywhere in the
engine. There is nothing to bypass and so nothing to be lying about; refusing
would break `browser.newContext({ bypassCSP: true })` over a capability the
page never had.

### D7. `attachedToTarget` precedes the `createTarget` reply

Chrome emits the attach while the target is being created, and Playwright
depends on that literally: `doCreateNewPage` reads
`_crPages.get(targetId)._page` the instant the reply lands. We left the attach
to the connection's event thread, which races the lane sending the reply. The
attach now happens on `createTarget`'s own lane, before it answers; both
threads still reach it, so a claim set decides which one attaches, and the lock
is held **across** the attach rather than only across the claim — a narrow lock
leaves the loser returning while the winner has not emitted yet, which is the
same race one level down.

This is the flake the roadmap recorded as `context.newPage` occasionally timing
out and reporting as sixteen failures. It was never a harness sensitivity.

### D8. A console message carries the context it was called from

`ConsoleMessage` gained a `context_id`, filled in exactly as `BindingCall`'s is
(ADR-0033 D10), and `Runtime.consoleAPICalled` reports it instead of guessing
the main context. This is correctness, not tidiness: a driver keys its context
map by id and **drops** any event naming an id it does not know, so a
utility-world `console.debug` attributed to the main context was silently
discarded. Playwright's `setContent` waits on exactly such a message.

### D9. `Network.setCacheDisabled` became real rather than staying refused

It used to be refused for `true` on the grounds that the HTTP cache is shared
browser-wide and has no per-page bypass. The premise was right and the
conclusion wrong: a per-*page* bypass needed nothing new, because every request
already carries a `bypass_cache` flag for `location.reload()`. The switch now
makes this page neither read from nor write to the shared cache, without
clearing it.

The reason it matters is the caller that sends it: every driver disables the
cache while intercepting, and a request served from cache **never reaches the
interceptor**. Accepting the command as a no-op would have silently lost
requests a driver had been promised it would see; refusing it broke
`page.route` outright.

### D10. `Page.navigatedWithinDocument` carries a `navigationType`

Classified at the commit, where the outgoing and incoming URLs are both still
known, and stored on the frame. The distinction is derived from the two URLs
rather than from how the navigation was requested: a change confined to the
fragment is `"fragment"`, anything else is `"historyApi"`. Chrome's third value,
`"other"`, is never emitted.

Two limits, both on `SameDocumentType` rather than left to be rediscovered:

- It mislabels a `pushState` that changes **only** the fragment, which is the
  one case the two-URL test cannot see.
- **`pushState`/`replaceState` emit no `Page.navigatedWithinDocument` at all**,
  because they record no navigation milestone — `shared_history_push` moves the
  document URL and the history entry without going through
  `commit_same_document`. So `"historyApi"` is reached in practice only by a
  same-document *traversal*. Chrome fires the event for both. This is a real
  gap for a single-page app whose router pushes state, and it is a page-crate
  change rather than a protocol one, so it is recorded here and left to the
  stage that wants it.

`Page.getFrameTree` also stopped answering with a frame of empty strings for a
target destroyed under a live session; it answers `no_target` instead. A
fabricated frame is the "fake" P6 forbids, and the window is narrow — the
`targetDestroyed` signal racing an in-flight command.

### D11. `page.waitForSelector` was a harness bug

Recorded so the diagnosis is not reassembled from scratch. `waitForSelector` is
a server-side retry loop in Playwright — no `pollRaf`, no `MutationObserver` —
whose default `state: 'visible'` requires a non-empty bounding box. Our check
appended an empty unstyled `<div>`. The fix is in `tests/playwright/run.mjs`;
no engine change was involved.

## Consequences

`cargo xtask playwright` is 17/17 with an empty `expectations.tsv`, and there
is a CI job for it. Puppeteer stays at 48/48 throughout, which is what made it
usable as the regression shield for every change above.

The costs taken on deliberately:

- **Two commit paths now exist** — one that renumbers contexts and one that
  does not. The distinction rides on `NavigationEvent::contexts_preserved`, and
  a third caller that gets it wrong will produce a driver that either drops
  every later event or rejects every command in flight. It is one bool, and it
  is the kind of bool that has to be right.
- **A deferred await is a `JsValue` living outside `WorldState`**, which is a
  new class of teardown hazard. Three flush points guard it and one test
  (`dropping_a_page_with_live_worlds_is_clean`) exercises it, but the failure
  mode is a process abort, so a fourth path that destroys a runtime must flush
  too.
- **A suspended page is now interruptible**, which is what makes it useful and
  also means "suspended" no longer implies "nothing runs". Anything added to
  the loop must decide which side of that line it is on.
- `document.open()` buffering rather than parsing incrementally is a visible
  deviation for pages that use it as a streaming API. It is the smaller half of
  the feature, and the half `setContent` needs is exact.
