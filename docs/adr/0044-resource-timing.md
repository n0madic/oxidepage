# ADR-0044: Resource Timing from the network event stream

- Status: accepted
- Date: 2026-11-04

## Context

`performance.getEntriesByType('resource')` always read empty. The
`performance` timeline held only the user-timing entries the bootstrap kept in
JS (`mark`/`measure`), as plain frozen objects with no `PerformanceEntry`
interface behind them. Pages and libraries use the resource timeline to wait
for, or audit, their subresources. WPT reads it too: `CSSStyleSheet-constructable`
checks that an `@import` dropped by `replaceSync` is never fetched.

The page has no single funnel where every fetch starts. Scripts, sheets,
`@import`, images, fonts, `fetch`/XHR and modules each start their own request,
on the blocking or the async path. There is one place that sees all of them,
though: the `NetworkEvent` stream `NetService::notify` raises for a driver's
`Network` domain (ADR-0030). It covers both paths, so the main document and
blocking `@import` fetches appear in it too.

## Decision

1. **The timeline is built from the network event stream.**
   `oxidepage_net::timing::TimingRecorder` pairs each request's `Requested`,
   `Responded` and `Finished` events into a `ResourceTiming`. It is fed from
   `notify` whether or not a driver installed an observer.
   - Document requests are not resource entries.
   - `data:` URLs are not resource entries either, matching browsers.
   - A failed request produces no entry.

   So the driver's request list and the page's resource timeline cannot
   disagree about which requests happened.

2. **An entry belongs to the browsing context that started the request.**
   The page installs a `TimingSink` that appends each entry to that context's
   `FrameShared` log. The sink resolves the context by the request's frame
   tag, and an untagged request belongs to the top-level context.
   - Every subresource fetch must therefore name its context. That includes
     the blocking ones: parser-blocking scripts, `@import` and a frame
     realm's module loads.
   - An untagged fetch from a frame would show a cross-origin frame's
     subresource URLs in its parent's timeline.
   - The log is bounded to 1024 entries, oldest first out.
   - Every entry carries a sequence number.
   - Each world's `performance` pulls the entries past its own cursor
     (`__oxide_resourceTimings`) into its own buffer.

   The buffer obeys the API's own controls:
   - 250 entries by default, changed with `setResourceTimingBufferSize`;
   - an entry that arrives once the buffer is full is dropped;
   - `clearResourceTimings` empties it.

   An entry that started before the current document's navigation belongs to
   the document the navigation replaced, and is skipped.

3. **Cross-origin detail follows the timing-allow check.**
   - An entry is detailed when the document's origin is not opaque and the
     request URL and final URL are both same-origin with it, or when the
     response's `Timing-Allow-Origin` is `*` or names the document's origin.
   - Otherwise the network phases, `responseStart`, `transferSize`,
     `encodedBodySize` and `decodedBodySize` read 0. Only `fetchStart`,
     `responseEnd`, the name and the initiator remain.
   - `Timing-Allow-Origin` is read off the **network** response, before the
     visibility filter, so `ResponseHead`, `NetEvent::Headers` and
     `NetworkEvent::Responded` gained a field for it. An opaque (`no-cors`)
     response shows script no headers at all, and the check has to see the
     real ones.
   - `responseStatus` and `contentType` follow CORS rather than the
     timing-allow check, as in Fetch. An opaque response already reports
     status 0 and no type.

4. **Timeline entries are interface instances.** The bootstrap defines:
   - `PerformanceEntry`;
   - `PerformanceMark`, which has the public constructor and `detail`;
   - `PerformanceMeasure`;
   - `PerformanceResourceTiming`.

   Each entry's state lives in a closure-held `WeakMap`, so a page cannot
   forge an entry by writing own properties. `getEntries*` returns user and
   resource entries together in chronological order.

## Consequences

- `performance.getEntriesByType('resource')` lists a page's stylesheets,
  scripts, images, fonts and `fetch`/XHR requests, each with its
  `initiatorType`. The three `CSSStyleSheet-constructable` subtests that read
  the timeline to prove an `@import` was never fetched now pass.
- **What the engine does not know reads as the fetch start, or as empty.**
  - There is no per-request DNS, connect or request-send timing.
  - `nextHopProtocol` is the empty string.
  - `redirectStart`/`redirectEnd` are 0 even after a redirect.
  - `transferSize` counts the body plus the header lines the response
    exposed, so an opaque response's headers are not counted.
- `initiatorType` comes from the request's type, not from the element or rule
  behind it:
  - an `@import` reads `"link"`, not `"css"`;
  - a font reads `"css"`;
  - an `<iframe>` document is not reported to its parent at all.
- `PerformanceObserver`, `resourcetimingbufferfull`, `serverTiming` contents
  and the navigation and paint entry types remain unimplemented. Feature
  detection for `PerformanceObserver` still answers no.
