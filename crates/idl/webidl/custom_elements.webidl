// Custom Elements (https://html.spec.whatwg.org/#custom-elements), autonomous
// elements only. `constructor`/`options` are typed `any` because the code
// generator passes them through as raw JS values; the imp module reads the
// callbacks and `observedAttributes` off the constructor itself.

interface CustomElementRegistry {
  [CEReactions] any define(DOMString name, any constructor, optional any options);
  any get(DOMString name);
  any getName(any constructor);
  any whenDefined(DOMString name);
  [CEReactions] undefined upgrade(any root);
};
