// SVG (https://svgwg.org/svg2-draft/), trimmed to what page script actually
// reaches for. Every element in the SVG namespace is wrapped as an
// `SVGElement`, so `el instanceof SVGElement` answers correctly — Vue's
// `createApp().mount()` brand-checks exactly that, unguarded, to pick the
// namespace it patches in, and a missing global there is a ReferenceError that
// takes the whole app down.
//
// `SVGAElement.href` is an `SVGAnimatedString`, not a string, and script
// branches on that (`e.href instanceof SVGAnimatedString ? e.href.animVal :
// e.href` is how VitePress reads a link target). It reflects the `href` content
// attribute live, falling back to the legacy `xlink:href`.
//
// The rest of the SVG DOM — SVGGraphicsElement and the other per-element
// interfaces, `ownerSVGElement`, `getBBox`, the animated-length/transform types
// — is *not* implemented, so it stays absent rather than half-faked (P6).
// Rendering of inline SVG does not go through these interfaces: it rasterizes
// the parsed subtree (ADR-0014).
interface SVGElement : Element {
  // The `HTMLOrSVGElement` mixin's `data-*` map (see `HTMLElement.dataset`).
  [SameObject] readonly attribute DOMStringMap dataset;
};

interface SVGAElement : SVGElement {
  [SameObject] readonly attribute SVGAnimatedString href;
};

// `animVal` equals `baseVal` here, which is what the spec prescribes whenever
// no animation is in effect — and SMIL animation is not implemented, so none
// ever is.
interface SVGAnimatedString {
  attribute DOMString baseVal;
  readonly attribute DOMString animVal;
};
