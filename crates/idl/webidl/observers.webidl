// Resize Observer (https://drafts.csswg.org/resize-observer/) and Intersection
// Observer (https://w3c.github.io/IntersectionObserver/). Entry objects are
// real interfaces, not plain objects, because polyfills feature-detect on their
// prototypes (`'intersectionRatio' in IntersectionObserverEntry.prototype`).
// Delivery is driven by the page event loop (ADR-0011), not rAF.

// === Resize Observer ===

callback ResizeObserverCallback = undefined (sequence<ResizeObserverEntry> entries, ResizeObserver observer);

enum ResizeObserverBoxOptions { "content-box", "border-box", "device-pixel-content-box" };

dictionary ResizeObserverOptions {
  ResizeObserverBoxOptions box = "content-box";
};

interface ResizeObserver {
  constructor(ResizeObserverCallback callback);
  undefined observe(Element target, optional ResizeObserverOptions options = {});
  undefined unobserve(Element target);
  undefined disconnect();
};

// `borderBoxSize`/`contentBoxSize`/`devicePixelContentBoxSize` are `any`: they
// return frozen arrays of plain `ResizeObserverSize` objects precomputed at
// delivery. `target`/`contentRect` are likewise precomputed JS values.
interface ResizeObserverEntry {
  readonly attribute any target;
  readonly attribute any contentRect;
  readonly attribute any borderBoxSize;
  readonly attribute any contentBoxSize;
  readonly attribute any devicePixelContentBoxSize;
};

// === Intersection Observer ===

callback IntersectionObserverCallback = undefined (sequence<IntersectionObserverEntry> entries, IntersectionObserver observer);

dictionary IntersectionObserverInit {
  any root = null;
  DOMString rootMargin = "0px";
  any threshold = 0;
};

// `root`/`thresholds` are `any` (an Element-or-null / a frozen double array);
// `rootMargin` is serialized back to a normalized string. v1 limits (ADR-0011):
// no clipping by intermediate overflow ancestors, no visibility/transform
// occlusion; a `Document` root is treated as the viewport.
interface IntersectionObserver {
  constructor(IntersectionObserverCallback callback, optional IntersectionObserverInit options = {});
  readonly attribute any root;
  readonly attribute DOMString rootMargin;
  readonly attribute any thresholds;
  undefined observe(Element target);
  undefined unobserve(Element target);
  undefined disconnect();
  sequence<IntersectionObserverEntry> takeRecords();
};

interface IntersectionObserverEntry {
  readonly attribute double time;
  readonly attribute any rootBounds;
  readonly attribute any boundingClientRect;
  readonly attribute any intersectionRect;
  readonly attribute boolean isIntersecting;
  readonly attribute double intersectionRatio;
  readonly attribute any target;
};
