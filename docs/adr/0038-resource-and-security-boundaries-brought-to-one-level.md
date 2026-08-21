# ADR-0038: Resource and security boundaries brought to one level

- Status: accepted
- Date: 2026-08-21
- Builds on: ADR-0029 (`data:` decoded above the scheme gate — its "the budget
  counters are not charged" is refined here), ADR-0027 (one OS thread per
  `Page`, the command timeout), ADR-0037 (a layout deadline: the same argument
  about what a budget is worth if a path can walk around it)
- Constrained by: design §2 P6 (absent beats fake), §8 (security posture and
  the SSRF filter), §12 (deliberate v1 limits)

## Context

An audit of the engine's resource and security boundaries against primary
sources — the IANA special-purpose address registries and RustSec's advisory
database — turned up one thing said five ways: **the engine has the boundaries
it advertises, but not every path reaches them at the same strictness.**

- `file://` returns from `fetch_inner` above the redirect loop, and the loop is
  where `charge_request` and `reserve_bytes` live. An opted-in local document
  could therefore pull an unbounded number of unbounded local files while
  `max_requests` and `max_total_bytes` read as enforced.
- `data:` returns at the same place, and additionally allocated its decoded body
  before any size was consulted.
- `ip_filter`'s IPv6 table had drifted from the registry: `2001::/32` covered
  Teredo but not the rest of the IETF protocol-assignments block, and
  `64:ff9b:1::/48`, `100::/64`, `3fff::/20`, `5f00::/16`, `fec0::/10` and IPv4's
  `192.88.99.0/24` were absent.
- The `file_root` jail checked a *path* (`canonicalize` + `starts_with`) and then
  read an inode it had opened earlier. The two are only the same object if
  nothing moved in between.
- `EngineError::Timeout` stopped the caller waiting but left the job queued, so
  the command still ran — later, at a moment nothing was expecting it.
- There was no supply-chain gate, and `cargo audit` on the checked-in
  `Cargo.lock` found two live advisories: `h2 0.4.15` (RUSTSEC-2026-0258,
  "unbounded empty DATA frames" — a DoS against exactly the fetch pipeline under
  discussion) and `crossbeam-epoch 0.9.18` (RUSTSEC-2026-0204). `rust-version =
  "1.89"` was a promise no job kept, and the workflow floated every action on a
  mutable tag.

Three tempting fixes are declined below with reasons: a bounded command queue,
generating the SSRF table from the registry, and widening `allowed_schemes` for
`data:` (which ADR-0029 already answered).

## Decision

### D1. `file://` is charged against the same budgets as HTTP

`FetchEngine::load_file` calls `charge_request()` and `reserve_bytes(None)`
before dispatching, and `commit`s the actual length after — the same three
existing primitives the HTTP path uses, no new accounting type. `reserve_bytes`
already takes `min(remaining headroom, max_response_bytes)`, so the cap handed
down is inside the per-request limit by construction, and `BudgetReservation`'s
`Drop` refunds the unused remainder.

`net::file::load_file` takes that cap and enforces it on the **read**, via
`Read::take(cap + 1)`, not on `metadata().len()`. A reported length is a hint
even for a regular file: it can change between the `stat` and the read, and
`/proc`-style files report zero while producing unbounded output. Reading one
byte past the limit distinguishes "exactly at the cap" from "over it" while
trusting no number.

Returning above the redirect loop is what put `file://` outside the loop's
charging; that placement is not a judgement that local bytes are free.

### D2. `data:` gets a per-URL cap, and deliberately not the cumulative ones

`net::data::decode` takes a **required** `max_bytes` parameter and returns
`Result<DataBody, DataError>` with `Malformed` and `TooLarge`. A required
parameter rather than an `Option` with a default: every entry point reached the
decoder from attacker-controlled markup, and a limit that can be omitted is one
that will be.

The check runs on the length of the *encoded* body, before the first allocation
proportional to it. Both transforms only shrink — percent-decoding collapses a
`%XX` triple to one byte, base64 four characters to three — so the encoded length
is a sound upper bound on the decoded one. It is a **loose** bound: a heavily
percent-encoded body just under the limit is refused even though its decoded form
would have fitted. That is what an allocate-nothing-first check costs, and it
errs toward refusing.

`max_requests` and `max_total_bytes` are **not** charged, and unlike `file://`
that is not an oversight. A `data:` body arrives inside the document that names
it, whose own bytes were already charged against `max_response_bytes`, and base64
expands 3 bytes to 4 — so the sum of every decoded `data:` body in a document is
structurally under `0.75 × document size`. Charging them again double-counts, and
rate-limiting them would drop a page's 501st inline icon, which no browser does.

`allowed_schemes` is **not** widened to include `data`, and ADR-0029 records
why: the early return is outside the redirect loop *on purpose*, and that is the
only thing keeping an `http:` → `data:` redirect a network error.

Two call sites outside `net` decode inline without entering the pipeline
(`start_image_load_url` and `start_font_load`, both in `crates/page`). Both read
the cap from `NetService::policy()` and treat `TooLarge` exactly as they already
treated a malformed body: a broken image, and a font source that did not load.

### D3. The SSRF table is brought to the IANA registry, and stays hand-written

Ranges are taken from the IANA IPv4 and IPv6 Special-Purpose Address Registries,
*Globally Reachable* column. Added: `192.88.99.0/24` (deprecated 6to4 relay
anycast); `64:ff9b:1::/48` (NAT64 local-use — distinct from `64:ff9b::/96`, which
is Globally Reachable and correctly recurses into its embedded IPv4);
`100::/64` and `100:0:0:1::/64`; `3fff::/20` (documentation, RFC 9637);
`5f00::/16` (SRv6 SIDs); `fec0::/10` (deprecated site-local — not in the registry
since RFC 3879, which nonetheless tolerates existing deployments and tells
*routers* to filter it; an SSRF boundary cannot delegate to somebody else's
router). `2001::/32` becomes `2001::/23`, one branch covering Teredo,
benchmarking and deprecated ORCHID.

The `/23` deliberately overblocks four Globally Reachable = True carve-outs
(`2001:1::1/128` PCP, `2001:3::/32` AMT, `2001:20::/28` ORCHIDv2, `2001:30::/28`
DET) — the same trade already made for `192.0.0.0/24`. None is a fetchable web
origin, and a coarse boundary is the one that stays correct as the registry
moves.

Generating the table from the registry is declined: the module is explicitly
designed to be hand-rolled so the blocked set is "explicit and testable", the
registry changes on the order of once a year, and a build-time fetch or a
vendored data file is more machinery than the problem. Each range
instead carries a boundary test — a blocked address *and* its reachable
neighbour.

### D4. The jail confirms the inode it opened, not just the path it vetted

`canonicalize` is a second path walk, so what it cleared is not necessarily what
`open` returned: swap a directory component for a symlink between the two and the
containment check passes on a path that no longer names the open inode. After the
`starts_with` check, `same_object(&file, &canonical)` compares `dev`+`ino` on
Unix and volume-serial-number + file-index on Windows (which `std` exposes only
on handle-derived `Metadata`, so that arm re-opens rather than `stat`s). Other
targets return `true` with a comment saying the jail degrades to path-only there.
No `unsafe`, no `openat2`, no new dependency — which is what workspace-wide
`unsafe_code = "deny"` requires.

**Left open, and recorded rather than implied away:** a hard link inside the jail
pointing at a file outside it defeats every path-based check. It has no link
target to resolve and its canonical path is genuinely inside the root. Closing
that needs a mount or user namespace, not a prefix comparison. `allow_file` is
off by default precisely because "jailed" means path-confined, not sandboxed.

### D5. `EngineError::Timeout` means the command was cancelled

`PageHandle::call_within` closes an `Arc<AtomicBool>` over the job. The job tests
it once, before running the closure; on timeout the caller raises it and then
re-checks the reply channel. The contract is one sentence, and it is now in the
variant's doc comment: **the command either did not run at all, or ran to
completion.** There is no partial state — cancelling mid-closure is what would
leave the page half-mutated, which is the thing this rules out. What the error
cannot say is *which* of the two happened, so a retry may perform the operation
twice and is safe only for work that tolerates that. No new error variant: the
distinction is not reliably available to the caller, so exposing it would be a
promise the type cannot keep.

The re-check after raising the flag matters. The job may have passed its test and
answered in the window between the wait expiring and the flag going up; reporting
`Timeout` while holding the finished result would be a lie. The reply wins.

The queue stays **unbounded**. Bounding it to apply backpressure is the obvious
alternative and is not available here, because `PageHandle::send` must never
block — some callers are themselves on a page thread with JavaScript on the stack
(the `window.open` hook), where a wait is one the `ScriptBudget` cannot interrupt.
A bounded queue would turn that into a deadlock. Cancellation is the pressure
that *is* available: a timed-out job is not removed from the queue, it is
neutered, and drains as a no-op.

`post()` is unchanged — it has no reply and therefore nothing to abandon.

Cancellation reaches **control** jobs too, and that sharpens one existing
asymmetry and blunts another. `PageHandle::resume` already cleared its
`suspended` mirror only on success, reasoning that a timed-out resume might not
have landed; now it *definitely* has not, so the mirror and the page agree.
`PageHandle::suspend` sets its mirror **before** queueing, so a cancelled suspend
leaves the mirror saying "suspended" over a page that is not. That was already a
divergence — it was merely transient, and is now durable. It is reachable only
from a page so wedged that even a control job finds no wait point, where the old
behavior (the suspend landing at some unpredictable later moment) was no more
useful. `request_close` is unaffected: it goes through `post_control`.

### D6. A supply-chain gate, SHA-pinned actions, and an MSRV job

`Cargo.lock` moves to `h2 0.4.18` and `crossbeam-epoch 0.9.20`. Both are
transitive patch bumps (via `hyper` and `crossbeam-deque`); no `=x.y.z` pin in
`[workspace.dependencies]` is touched. Three new pieces of CI:

- an **`audit`** job (`cargo install cargo-audit --locked` + `cargo audit`),
  which reads only `Cargo.lock` and so needs neither python3 nor fontconfig;
- an **`msrv`** job running `cargo check --workspace --all-targets` on 1.89.0.
  `check`, not `test`: it answers whether the declared MSRV compiles the
  workspace, and behavior is already covered on stable across three platforms;
- every action pinned to a commit SHA with the version in a trailing comment,
  and `.github/dependabot.yml` (cargo + github-actions, weekly) to move those
  pins, since a pin does not float on its own.

`dtolnay/rust-toolchain` gets an explicit `toolchain:` at every use. `@stable`
worked because that ref is a *branch* whose own `action.yml` defaults the input
to `stable`; the pinned SHA is that branch's tip and carries the same default, so
this is belt-and-braces — but it is what keeps the job honest if the pin is ever
moved. The `msrv` job reuses the same SHA and overrides the input.

`cargo deny` is **not** added here. It is not installed locally, and a config
that cannot be run before it is committed is a request for a red CI. Follow-up.

## Consequences

- **Every fetch path now passes through the same two budgets**, so `max_requests`
  and `max_total_bytes` describe the engine rather than describing HTTP. The
  behavioral blast radius is small and checkable: `allow_file` is off by default,
  and the CLI reads the main local document directly through `std::fs`, not
  through the fetch pipeline — only subresources of a `file://` document with
  `allow_file` explicitly on come under the new counter.
- **A `data:` bomb is refused before it is allocated**, not after. The refusal is
  conservative on percent-encoded bodies (D2), which is the direction to be wrong
  in.
- **`decode`'s signature changed from `Option` to `Result` with a required
  limit.** That is deliberate friction: a new call site cannot compile without
  deciding what its limit is. The cost is that the `Option`-shaped ergonomics are
  gone from ~30 existing test call sites and two production ones.
- **The SSRF filter overblocks four small Globally Reachable ranges** (D3) and
  will drift again as the registry moves. The boundary tests make the drift
  visible when someone next reads the file; they do not prevent it. Generating
  the table is the fix if that ever stops being true.
- **The jail's guarantee is now "the bytes are from the inode we vetted"**, which
  is strictly stronger than "the path we vetted was inside the root" — and still
  not "the file is inside the root", because of hard links. Both halves of that
  are stated in the module doc, so the gap does not read as closed.
- **`EngineError::Timeout` is safe to report to a driver.** Before, a timed-out
  `Page.navigate` could commit a navigation after the protocol had already
  answered with an error. There is no regression test for the *old* behavior
  because there was no test for `command_timeout` at all: `tests/common/mod.rs`
  set it to a generous 20 s precisely so it would never fire. There are two
  tests now — a unit test for the race window and an integration test for the
  cancel-before-start path.
- **CI grows two jobs.** `audit` is seconds; `msrv` is a full workspace
  type-check on a second toolchain, so it will not share `rust-cache` with the
  stable jobs and costs a cold-ish build. That is the price of the promise in
  `rust-version` meaning something.
- **`cargo audit` can go red without anyone changing this repository**, the day
  an advisory lands against a transitive dependency. That is the job working, and
  the fix is a lockfile bump, never an ignore entry.
- **Not addressed:** peak layout/DOM memory is still unbounded —
  `docs/status.md` records the `repeat(auto-fill, 1px)` → ~431 MB case
  honestly, and this changes none of it. Bounding it means
  process-per-page isolation, which is a change of execution model and wants its
  own ADR.
- None of this changes behavior visible to WPT, the display-list goldens, the
  reftests, or the Puppeteer and Playwright suites; all four expectation files
  are byte-identical.
