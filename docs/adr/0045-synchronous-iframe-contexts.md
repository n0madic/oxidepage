# ADR-0045: An inserted `<iframe>` gets its browsing context synchronously

- Status: accepted
- Date: 2026-11-04

## Context

HTML creates an `<iframe>`'s browsing context, with its initial `about:blank`
document, during the insertion steps. So the most common way to get a fresh
same-origin document works on the very next line:

```js
document.body.appendChild(iframe);
iframe.contentDocument.body.appendChild(…);
```

Libraries use it to build an isolated document or to get a pristine realm, and
WPT uses it throughout (`Node-isConnected`'s "Test with iframes" is one).

ADR-0035 made the context a task instead. `DomTree` queued
`FrameUpdate::Connected`, and `Page::drain_frame_updates` attached the frame
on the loop's next turn. That was deliberate: building a context evaluates
`bootstrap.js` in a fresh realm and runs the driver's init scripts, and only
the page can do that. `bindings` sees neither the page nor its world table,
and the DOM mutation that queues the update is still holding its borrows. So
`contentDocument` read `null` until the script yielded.

The host-call trampoline (`cx::native`) already has a point where a mutating
call has released every borrow and no further JS has run. Inline scripts and
custom element reactions already run from there. What was missing was a way
to reach the page from that point.

## Decision

1. **`bindings` asks, and the page does it.** `bindings` defines a
   `FrameHost` trait with a single method, `attach_connected_frames`. Its
   `PageGlobal` holds a `Weak<dyn FrameHost>`, for the same cycle reason as
   `WorldEnter`. After a successful host call, the trampoline calls it when
   `DomTree::has_pending_frame_connect` says an `<iframe>` was connected. It
   makes that call after the custom element reactions, so an `<iframe>` a
   `connectedCallback` inserted is included.

2. **The page is found through a scoped thread-local.**
   - `ENTERED_PAGE` is set for the extent of every `Page::with_cx` and
     `Page::with_cx_in`. These are the page's only entries into JS, so every
     host call runs inside one of them.
   - It is a `scoped-tls` `scoped_thread_local!`, so it holds a plain
     `&Page` that is valid only while that borrow is. Nothing can observe it
     afterwards, and our code still contains no `unsafe`.
   - The registered `FrameHost` is stateless. With no page entered, the
     request is dropped and the queued update creates the context a turn
     later, as before.
   - The alternative was to turn `Page` into a handle over an
     `Rc<PageCore>`, so a `Weak` to it could exist. That changes the public
     type every embedder and every `PageJob` names, all for one call site.

3. **Only the context is created early.**
   - `Page::attach_connected_frames_now` attaches each connected `<iframe>`
     still in the queue through the ordinary `attach_child_frame`: the same
     document, engines, realm, sandbox flags, init scripts and `Attached`
     event as the task path.
   - It records each such `<iframe>` in `attached_early`. The queued
     `Connected` entry stays put, and its drain finds the context already
     there and owes only the initial navigation to `src`/`srcdoc` and the pin
     release.
   - The navigation remains a task, and so does the `load` an `<iframe>`
     without `src` fires, exactly as ADR-0035 had them.

4. **No early attach while a context is being built.** `attaching_frame` is
   set around every `attach_child_frame`. An `<iframe>` inserted by an init
   script of a frame under construction waits for the drain. Building it
   then would report the child's `Attached` to a driver before its parent's,
   and Puppeteer and Playwright both look the parent up.

5. **`WindowProxy.document` exists.** For a same-origin frame it is the frame's
   document in the accessing realm's wrapper, the same node
   `contentDocument` returns. Otherwise it throws `SecurityError`. The rest of
   a child's globals stay unreachable (ADR-0035 D4).

## Consequences

- `appendChild(iframe)` followed by `iframe.contentDocument`,
  `iframe.contentWindow` or `contentWindow.document` reads the initial
  document. That includes nested frames inserted into that document from the
  same script.
- **New dependency: `scoped-tls`.** It is tiny, `std`-only and long stable,
  and it carries the one `unsafe` the pattern needs, so ours stays at zero.
- Removal is still a task. A removed `<iframe>` keeps answering
  `contentDocument` until the next turn, and an `<iframe>` removed and
  reinserted in one script shows its old context until the drain replaces it.
- Calling `contentDocument.open()`/`write()` from the embedder's realm is
  still refused. The frame's parser belongs to the frame's realm.
- A frame's initial document still reports its embedder's URL rather than
  `about:blank` (ADR-0035).
