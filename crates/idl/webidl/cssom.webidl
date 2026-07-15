// CSSOM (https://drafts.csswg.org/cssom/), trimmed to Phase 4 (design doc §10,
// ADR-0005). Declares `CSSStyleDeclaration` (backing `el.style`,
// `getComputedStyle`, and rule declarations), the document stylesheet set, and
// the style-rule surface. `@media`/`@import`/`@supports` parse and cascade, but
// their CSSOM wrappers surface as the base `CSSRule` in v1 (MediaList and the
// grouping-rule interfaces are deferred — "absent beats fake").
//
// camelCase/dashed property accessors and indexed access on
// `CSSStyleDeclaration` and the list interfaces are provided by the `styleProxy`
// / `collectionProxy` bootstrap wrappers, not generated here.

interface CSSStyleDeclaration {
  [CEReactions] attribute DOMString cssText;
  readonly attribute unsigned long length;
  getter DOMString item(unsigned long index);
  DOMString getPropertyValue(DOMString property);
  DOMString getPropertyPriority(DOMString property);
  [CEReactions] undefined setProperty(DOMString property, DOMString value, optional DOMString priority = "");
  [CEReactions] DOMString removeProperty(DOMString property);
  readonly attribute CSSRule? parentRule;
};

interface StyleSheet {
  readonly attribute DOMString type;
  readonly attribute DOMString? href;
  readonly attribute Element? ownerNode;
  readonly attribute CSSStyleSheet? parentStyleSheet;
  readonly attribute DOMString? title;
  attribute boolean disabled;
};

interface CSSStyleSheet : StyleSheet {
  constructor(optional any options);
  readonly attribute CSSRule? ownerRule;
  readonly attribute CSSRuleList cssRules;
  unsigned long insertRule(DOMString rule, optional unsigned long index = 0);
  undefined deleteRule(unsigned long index);
  any replace(DOMString text);
  undefined replaceSync(DOMString text);
};

interface StyleSheetList {
  getter CSSStyleSheet? item(unsigned long index);
  readonly attribute unsigned long length;
};

interface CSSRuleList {
  getter CSSRule? item(unsigned long index);
  readonly attribute unsigned long length;
};

interface CSSRule {
  const unsigned short STYLE_RULE = 1;
  const unsigned short CHARSET_RULE = 2;
  const unsigned short IMPORT_RULE = 3;
  const unsigned short MEDIA_RULE = 4;
  const unsigned short FONT_FACE_RULE = 5;
  const unsigned short PAGE_RULE = 6;
  const unsigned short KEYFRAMES_RULE = 7;
  const unsigned short KEYFRAME_RULE = 8;
  const unsigned short MARGIN_RULE = 9;
  const unsigned short NAMESPACE_RULE = 10;
  const unsigned short COUNTER_STYLE_RULE = 11;
  const unsigned short SUPPORTS_RULE = 12;
  const unsigned short FONT_FEATURE_VALUES_RULE = 14;

  readonly attribute unsigned short type;
  attribute DOMString cssText;
  readonly attribute CSSRule? parentRule;
  readonly attribute CSSStyleSheet? parentStyleSheet;
};

interface CSSStyleRule : CSSRule {
  attribute DOMString selectorText;
  [SameObject] readonly attribute CSSStyleDeclaration style;
};

partial interface Document {
  [SameObject] readonly attribute StyleSheetList styleSheets;
  attribute any adoptedStyleSheets;
};
