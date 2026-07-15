// Geometry Interfaces (https://drafts.fxtf.org/geometry/), Phase 5 surface: the
// `DOMRect` family used by the layout geometry APIs (getBoundingClientRect,
// getClientRects). `DOMRectInit` is a plain object read by hand (all members
// default to 0), so no dictionary is declared here.
//
// `fromRect` is a static operation on both interfaces; the codegen does not
// emit static ops, so both are hand-registered in `install_dom_rect_statics`
// (like `URL.parse`). `toJSON()` is expressed as `any` — the imp returns a
// finished plain object with all eight members.

interface DOMRectReadOnly {
  constructor(optional unrestricted double x = 0, optional unrestricted double y = 0,
              optional unrestricted double width = 0, optional unrestricted double height = 0);

  readonly attribute unrestricted double x;
  readonly attribute unrestricted double y;
  readonly attribute unrestricted double width;
  readonly attribute unrestricted double height;
  readonly attribute unrestricted double top;
  readonly attribute unrestricted double right;
  readonly attribute unrestricted double bottom;
  readonly attribute unrestricted double left;

  any toJSON();
};

interface DOMRect : DOMRectReadOnly {
  constructor(optional unrestricted double x = 0, optional unrestricted double y = 0,
              optional unrestricted double width = 0, optional unrestricted double height = 0);

  attribute unrestricted double x;
  attribute unrestricted double y;
  attribute unrestricted double width;
  attribute unrestricted double height;
};

interface DOMRectList {
  readonly attribute unsigned long length;
  getter DOMRect? item(unsigned long index);
};

// CSSOM View Module (https://drafts.csswg.org/cssom-view/), Phase 5 surface.
// Layout-backed geometry: every member below flushes pending style updates
// and reflows before reading (`imp::geometry_support::flush_layout`).
//
// `scroll`/`scrollTo`/`scrollBy` are part of this surface too, but are *not*
// declared here: each is spec'd as a two-form overload (unrestricted double
// x, y | optional ScrollToOptions), which the codegen does not support. They
// are hand-registered directly on `Element.prototype` in `install_cssom`
// (`crates/bindings/src/lib.rs`), mirroring `Window.scroll`/`scrollTo`/
// `scrollBy`.

dictionary CheckVisibilityOptions {
  boolean checkOpacity = false;
  boolean checkVisibilityCSS = false;
  boolean contentVisibilityAuto = false;
  boolean opacityProperty = false;
  boolean visibilityProperty = false;
};

partial interface Element {
  DOMRectList getClientRects();
  DOMRect getBoundingClientRect();

  attribute unrestricted double scrollTop;
  attribute unrestricted double scrollLeft;
  readonly attribute long scrollWidth;
  readonly attribute long scrollHeight;
  readonly attribute long clientTop;
  readonly attribute long clientLeft;
  readonly attribute long clientWidth;
  readonly attribute long clientHeight;

  boolean checkVisibility(optional CheckVisibilityOptions options = {});

  // `scrollParent` is declared as a *method* here, not the `readonly
  // attribute Element? scrollParent` the current editor's draft shows —
  // every vendored WPT file (tests/wpt/vendor/css/cssom-view/scrollParent*)
  // calls it as `element.scrollParent()`, and that vendored copy is what
  // conformance is measured against here (P7).
  Element? scrollParent();
};

partial interface HTMLElement {
  readonly attribute Element? offsetParent;
  readonly attribute long offsetTop;
  readonly attribute long offsetLeft;
  readonly attribute long offsetWidth;
  readonly attribute long offsetHeight;
};

partial interface Document {
  Element? elementFromPoint(double x, double y);
  sequence<Element> elementsFromPoint(double x, double y);
  readonly attribute Element? scrollingElement;
};
