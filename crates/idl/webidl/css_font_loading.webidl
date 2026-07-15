// CSS Font Loading Module Level 3 (https://drafts.csswg.org/css-font-loading/),
// trimmed to the minimal *real* subset the vendored WPT corpus needs:
// `document.fonts.ready` and `.status`. Several cssom-view/cssom tests do
// `document.fonts.ready.then(() => { ...the actual assertions... })` before
// running, so without this they die on `document.fonts` being `undefined` or
// hang forever waiting on a promise that never resolves.
//
// `add`/`delete`/`clear`/`check`/iteration are not declared (P6 "absent beats
// fake") — nothing vendored exercises them, and a half-faked `FontFaceSet`
// (e.g. an always-empty iterable) would be worse than absent.
//
// `load(font, text)` *is* declared: angular.dev calls `document.fonts.load(...)`
// unconditionally (no feature detection), so its absence throws `not a function`
// mid-render and aborts the app. The engine already fetches the matching family
// through its `@font-face` rule, so `load` resolves on the same settle condition
// as `ready` (the returned `sequence<FontFace>` is empty — callers await
// completion, not the list). Return type is `any` for the same reason `ready`
// is: it sidesteps the `Promise<T>` / `FontFace` codegen machinery for a value
// the impl builds directly.
interface FontFaceSet {
  readonly attribute any ready;
  readonly attribute DOMString status;
  any load(DOMString font, optional DOMString text = " ");
};

partial interface Document {
  [SameObject] readonly attribute any fonts;
};
