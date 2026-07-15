// HTML Standard additions (https://html.spec.whatwg.org/), trimmed to a
// practical surface. `HTMLElement` still backs every tag that has no per-tag
// interface below; `crates/bindings/src/cx.rs::html_interface_for` is the map
// from tag to interface, and adding an interface here means adding it there.
// `Document.body`/`head` are read-only in Phase 2 (setter arrives with the
// full HTML surface); `defaultView` is typed `any` because the code generator
// does not yet return interface-typed attributes.

interface Window : EventTarget {
  any matchMedia(DOMString query);
};

// Event handler IDL attributes (HTML §8.1.7). `EventHandler` is a type the
// codegen knows by name: it emits the accessor pair directly against the
// handler registry, deriving the event type from the member name, so none of
// these needs a hand-written `imp` function. The set below is also the single
// source of truth for which `on*` *content* attributes are handlers — codegen
// exports it as `EVENT_HANDLER_TYPES`, which `handlers.rs` consumes, so the two
// halves of HTML's "install a handler" cannot drift apart.
interface mixin GlobalEventHandlers {
  attribute EventHandler onabort;
  attribute EventHandler onauxclick;
  attribute EventHandler onbeforeinput;
  attribute EventHandler onbeforematch;
  attribute EventHandler onbeforetoggle;
  attribute EventHandler onblur;
  attribute EventHandler oncancel;
  attribute EventHandler oncanplay;
  attribute EventHandler oncanplaythrough;
  attribute EventHandler onchange;
  attribute EventHandler onclick;
  attribute EventHandler onclose;
  attribute EventHandler oncontextlost;
  attribute EventHandler oncontextmenu;
  attribute EventHandler oncontextrestored;
  attribute EventHandler oncopy;
  attribute EventHandler oncuechange;
  attribute EventHandler oncut;
  attribute EventHandler ondblclick;
  attribute EventHandler ondrag;
  attribute EventHandler ondragend;
  attribute EventHandler ondragenter;
  attribute EventHandler ondragleave;
  attribute EventHandler ondragover;
  attribute EventHandler ondragstart;
  attribute EventHandler ondrop;
  attribute EventHandler ondurationchange;
  attribute EventHandler onemptied;
  attribute EventHandler onended;
  attribute EventHandler onerror;
  attribute EventHandler onfocus;
  attribute EventHandler onformdata;
  attribute EventHandler oninput;
  attribute EventHandler oninvalid;
  attribute EventHandler onkeydown;
  attribute EventHandler onkeypress;
  attribute EventHandler onkeyup;
  attribute EventHandler onload;
  attribute EventHandler onloadeddata;
  attribute EventHandler onloadedmetadata;
  attribute EventHandler onloadstart;
  attribute EventHandler onmousedown;
  attribute EventHandler onmouseenter;
  attribute EventHandler onmouseleave;
  attribute EventHandler onmousemove;
  attribute EventHandler onmouseout;
  attribute EventHandler onmouseover;
  attribute EventHandler onmouseup;
  attribute EventHandler onpaste;
  attribute EventHandler onpause;
  attribute EventHandler onplay;
  attribute EventHandler onplaying;
  attribute EventHandler onprogress;
  attribute EventHandler onratechange;
  attribute EventHandler onreset;
  attribute EventHandler onresize;
  attribute EventHandler onscroll;
  attribute EventHandler onscrollend;
  attribute EventHandler onsecuritypolicyviolation;
  attribute EventHandler onseeked;
  attribute EventHandler onseeking;
  attribute EventHandler onselect;
  attribute EventHandler onslotchange;
  attribute EventHandler onstalled;
  attribute EventHandler onsubmit;
  attribute EventHandler onsuspend;
  attribute EventHandler ontimeupdate;
  attribute EventHandler ontoggle;
  attribute EventHandler onvolumechange;
  attribute EventHandler onwaiting;
  attribute EventHandler onwheel;
};
HTMLElement includes GlobalEventHandlers;
Document includes GlobalEventHandlers;
Window includes GlobalEventHandlers;

// Handlers that only ever live on the Window. On `<body>`/`<frameset>` these —
// plus the "Window-reflecting body element event handler set" (blur, error,
// focus, load, resize, scroll) — reflect onto the Window instead of the element;
// `handlers.rs::WINDOW_REFLECTED` is that combined list.
interface mixin WindowEventHandlers {
  attribute EventHandler onafterprint;
  attribute EventHandler onbeforeprint;
  attribute EventHandler onbeforeunload;
  attribute EventHandler onhashchange;
  attribute EventHandler onlanguagechange;
  attribute EventHandler onmessage;
  attribute EventHandler onmessageerror;
  attribute EventHandler onoffline;
  attribute EventHandler ononline;
  attribute EventHandler onpagehide;
  attribute EventHandler onpageshow;
  attribute EventHandler onpopstate;
  attribute EventHandler onrejectionhandled;
  attribute EventHandler onstorage;
  attribute EventHandler onunhandledrejection;
  attribute EventHandler onunload;
};
Window includes WindowEventHandlers;
HTMLBodyElement includes WindowEventHandlers;

interface HTMLElement : Element {
  constructor();
  [CEReactions] attribute DOMString dir;
  [SameObject] readonly attribute DOMStringMap dataset;
  undefined click();
  undefined focus();
  undefined blur();
};

// The `data-*` attribute map behind `element.dataset` (HTML "domstringmap").
// No members: the whole surface is the named getter/setter/deleter, provided by
// the `datasetProxy` bootstrap wrapper over the element's attribute methods.
interface DOMStringMap {
};

interface HTMLUnknownElement : HTMLElement {
};

// Per-tag interfaces exposed for feature detection / `instanceof` / subclassing.
// v1 gives them no members yet (they behave like `HTMLElement`); the point is
// that the globals exist and `document.createElement(tag) instanceof HTMLXElement`
// holds (tag→interface wiring in `cx.rs::html_interface_for`). Constructing one
// directly (`new HTMLInputElement()`) is an illegal constructor, as in browsers.
interface HTMLHtmlElement : HTMLElement {};
interface HTMLHeadElement : HTMLElement {};
interface HTMLBodyElement : HTMLElement {};
interface HTMLTitleElement : HTMLElement {};
interface HTMLMetaElement : HTMLElement {};
interface HTMLBaseElement : HTMLElement {};
interface HTMLStyleElement : HTMLElement {};
interface HTMLTemplateElement : HTMLElement {
  [SameObject] readonly attribute DocumentFragment content;
};
interface HTMLDivElement : HTMLElement {};
interface HTMLSpanElement : HTMLElement {};
interface HTMLParagraphElement : HTMLElement {};
interface HTMLHeadingElement : HTMLElement {};
interface HTMLPreElement : HTMLElement {};
interface HTMLQuoteElement : HTMLElement {};
interface HTMLBRElement : HTMLElement {};
interface HTMLHRElement : HTMLElement {};
interface HTMLUListElement : HTMLElement {};
interface HTMLOListElement : HTMLElement {};
interface HTMLLIElement : HTMLElement {};
interface HTMLDListElement : HTMLElement {};
interface HTMLLegendElement : HTMLElement {};
interface HTMLTableElement : HTMLElement {};
interface HTMLTableSectionElement : HTMLElement {};
interface HTMLTableRowElement : HTMLElement {};
interface HTMLTableCellElement : HTMLElement {};
interface HTMLTableColElement : HTMLElement {};
interface HTMLTableCaptionElement : HTMLElement {};
interface HTMLIFrameElement : HTMLElement {};
interface HTMLCanvasElement : HTMLElement {};
interface HTMLPictureElement : HTMLElement {};
interface HTMLSourceElement : HTMLElement {};
interface HTMLMediaElement : HTMLElement {};
interface HTMLVideoElement : HTMLMediaElement {};
interface HTMLAudioElement : HTMLMediaElement {};
interface HTMLTrackElement : HTMLElement {};
interface HTMLObjectElement : HTMLElement {};
interface HTMLEmbedElement : HTMLElement {};
interface HTMLMapElement : HTMLElement {};
interface HTMLDataListElement : HTMLElement {};
interface HTMLOutputElement : HTMLElement {};
interface HTMLProgressElement : HTMLElement {};
interface HTMLMeterElement : HTMLElement {};
interface HTMLDetailsElement : HTMLElement {};
interface HTMLDialogElement : HTMLElement {};
interface HTMLMenuElement : HTMLElement {};
interface HTMLTimeElement : HTMLElement {};
interface HTMLDataElement : HTMLElement {};
interface HTMLModElement : HTMLElement {};
interface HTMLSlotElement : HTMLElement {
  [CEReactions] attribute DOMString name;
  sequence<Node> assignedNodes(optional any options);
  sequence<Element> assignedElements(optional any options);
};

interface HTMLScriptElement : HTMLElement {
  [CEReactions] attribute USVString src;
  [CEReactions] attribute DOMString type;
  [CEReactions] attribute boolean async;
  [CEReactions] attribute boolean defer;
  [CEReactions] attribute boolean noModule;
  [CEReactions] attribute DOMString? crossOrigin;
  [CEReactions] attribute DOMString text;
};

// The `href` + URL-decomposition surface of `<a>` and `<area>`. Mixin members
// register on each including interface's prototype but share one imp module.
interface mixin HTMLHyperlinkElementUtils {
  stringifier attribute USVString href;
  readonly attribute USVString origin;
  attribute USVString protocol;
  attribute USVString username;
  attribute USVString password;
  attribute USVString host;
  attribute USVString hostname;
  attribute USVString port;
  attribute USVString pathname;
  attribute USVString search;
  attribute USVString hash;
};

interface HTMLAnchorElement : HTMLElement {
  [CEReactions] attribute DOMString target;
  [CEReactions] attribute DOMString download;
  [CEReactions] attribute DOMString rel;
  [SameObject, PutForwards=value] readonly attribute DOMTokenList relList;
  [CEReactions] attribute DOMString hreflang;
  [CEReactions] attribute DOMString type;
  [CEReactions] attribute DOMString text;
  [CEReactions] attribute DOMString referrerPolicy;
};
HTMLAnchorElement includes HTMLHyperlinkElementUtils;

interface HTMLAreaElement : HTMLElement {
  [CEReactions] attribute DOMString alt;
  [CEReactions] attribute DOMString coords;
  [CEReactions] attribute DOMString shape;
  [CEReactions] attribute DOMString target;
  [CEReactions] attribute DOMString download;
  [CEReactions] attribute DOMString rel;
  [SameObject, PutForwards=value] readonly attribute DOMTokenList relList;
  [CEReactions] attribute DOMString referrerPolicy;
};
HTMLAreaElement includes HTMLHyperlinkElementUtils;

interface HTMLImageElement : HTMLElement {
  [CEReactions] attribute USVString src;
  [CEReactions] attribute USVString srcset;
  [CEReactions] attribute DOMString alt;
  [CEReactions] attribute unsigned long width;
  [CEReactions] attribute unsigned long height;
  readonly attribute unsigned long naturalWidth;
  readonly attribute unsigned long naturalHeight;
  readonly attribute boolean complete;
  readonly attribute USVString currentSrc;
  [CEReactions] attribute DOMString loading;
  [CEReactions] attribute DOMString decoding;
  [CEReactions] attribute DOMString? crossOrigin;
  [CEReactions] attribute DOMString referrerPolicy;
};

interface HTMLLinkElement : HTMLElement {
  [CEReactions] attribute USVString href;
  [CEReactions] attribute DOMString rel;
  [SameObject, PutForwards=value] readonly attribute DOMTokenList relList;
  [CEReactions] attribute DOMString media;
  [CEReactions] attribute DOMString type;
  [CEReactions] attribute DOMString as;
  [CEReactions] attribute boolean disabled;
  [CEReactions] attribute DOMString? crossOrigin;
  [CEReactions] attribute DOMString hreflang;
};

// ---------------------------------------------------------------------------
// Form controls (HTML §4.10).
//
// `value`, `checked`, `selected` and `indeterminate` are deliberately NOT
// reflected attributes: they are backed by the control's form state, whose
// dirty flags decouple them from the content attribute the moment script writes
// one. The `default*` members are the reflecting half of each pair. See
// `oxidepage_dom::form`.
// ---------------------------------------------------------------------------

interface HTMLFormElement : HTMLElement {
  [CEReactions] attribute USVString action;
  [CEReactions] attribute DOMString method;
  [CEReactions] attribute DOMString enctype;
  [CEReactions] attribute DOMString target;
  [CEReactions] attribute DOMString name;
  [CEReactions] attribute boolean noValidate;
  [CEReactions] attribute DOMString acceptCharset;
  [SameObject] readonly attribute HTMLCollection elements;
  readonly attribute unsigned long length;
  undefined reset();
};

interface HTMLInputElement : HTMLElement {
  [CEReactions] attribute DOMString type;
  [CEReactions] attribute DOMString name;
  [CEReactions] attribute DOMString placeholder;
  [CEReactions] attribute boolean disabled;
  [CEReactions] attribute boolean readOnly;
  [CEReactions] attribute boolean required;
  [CEReactions] attribute boolean multiple;
  [CEReactions] attribute DOMString defaultValue;
  [CEReactions] attribute boolean defaultChecked;
  attribute DOMString value;
  attribute boolean checked;
  attribute boolean indeterminate;
  readonly attribute HTMLFormElement? form;
  [SameObject] readonly attribute NodeList labels;
};

interface HTMLTextAreaElement : HTMLElement {
  [CEReactions] attribute DOMString name;
  [CEReactions] attribute DOMString placeholder;
  [CEReactions] attribute boolean disabled;
  [CEReactions] attribute boolean readOnly;
  [CEReactions] attribute boolean required;
  [CEReactions] attribute unsigned long rows;
  [CEReactions] attribute unsigned long cols;
  [CEReactions] attribute DOMString defaultValue;
  attribute DOMString value;
  readonly attribute unsigned long textLength;
  readonly attribute DOMString type;
  readonly attribute HTMLFormElement? form;
  [SameObject] readonly attribute NodeList labels;
};

interface HTMLSelectElement : HTMLElement {
  [CEReactions] attribute DOMString name;
  [CEReactions] attribute boolean disabled;
  [CEReactions] attribute boolean required;
  [CEReactions] attribute boolean multiple;
  [CEReactions] attribute unsigned long size;
  attribute DOMString value;
  attribute long selectedIndex;
  readonly attribute unsigned long length;
  readonly attribute DOMString type;
  [SameObject] readonly attribute HTMLCollection options;
  [SameObject] readonly attribute HTMLCollection selectedOptions;
  readonly attribute HTMLFormElement? form;
  [SameObject] readonly attribute NodeList labels;
};

interface HTMLOptionElement : HTMLElement {
  [CEReactions] attribute boolean disabled;
  [CEReactions] attribute DOMString label;
  [CEReactions] attribute boolean defaultSelected;
  attribute DOMString value;
  attribute DOMString text;
  attribute boolean selected;
  readonly attribute long index;
  readonly attribute HTMLFormElement? form;
};

interface HTMLOptGroupElement : HTMLElement {
  [CEReactions] attribute boolean disabled;
  [CEReactions] attribute DOMString label;
};

interface HTMLButtonElement : HTMLElement {
  [CEReactions] attribute DOMString type;
  [CEReactions] attribute DOMString name;
  [CEReactions] attribute DOMString value;
  [CEReactions] attribute boolean disabled;
  readonly attribute HTMLFormElement? form;
  [SameObject] readonly attribute NodeList labels;
};

interface HTMLLabelElement : HTMLElement {
  [CEReactions] attribute DOMString htmlFor;
  readonly attribute HTMLElement? control;
  readonly attribute HTMLFormElement? form;
};

interface HTMLFieldSetElement : HTMLElement {
  [CEReactions] attribute DOMString name;
  [CEReactions] attribute boolean disabled;
  readonly attribute DOMString type;
  [SameObject] readonly attribute HTMLCollection elements;
  readonly attribute HTMLFormElement? form;
};

interface Navigator {
  readonly attribute DOMString appCodeName;
  readonly attribute DOMString appName;
  readonly attribute DOMString appVersion;
  readonly attribute DOMString platform;
  readonly attribute DOMString product;
  readonly attribute DOMString productSub;
  readonly attribute DOMString userAgent;
  readonly attribute DOMString vendor;
  readonly attribute DOMString vendorSub;
  readonly attribute DOMString language;
  readonly attribute any languages;
  readonly attribute boolean onLine;
  readonly attribute boolean cookieEnabled;
  readonly attribute unsigned long long hardwareConcurrency;
  readonly attribute boolean webdriver;
  readonly attribute unsigned long maxTouchPoints;
  readonly attribute boolean pdfViewerEnabled;
  readonly attribute any plugins;
  readonly attribute any mimeTypes;
  boolean javaEnabled();
};

interface Screen {
  readonly attribute unsigned long width;
  readonly attribute unsigned long height;
  readonly attribute unsigned long availWidth;
  readonly attribute unsigned long availHeight;
  readonly attribute unsigned long colorDepth;
  readonly attribute unsigned long pixelDepth;
};

interface Performance {
  readonly attribute double timeOrigin;
  double now();
  [SameObject] readonly attribute PerformanceTiming timing;
};

// Legacy Navigation Timing Level 1. Values are Unix-epoch milliseconds; a
// milestone not yet reached reads 0. `unload*`/`redirect*`/
// `secureConnectionStart` are hardcoded 0 (v1: no distinct network phases for
// injected HTML). `mark`/`measure`/`getEntries*` are hand-installed
// (`installLateGlobals`).
interface PerformanceTiming {
  readonly attribute unsigned long long navigationStart;
  readonly attribute unsigned long long unloadEventStart;
  readonly attribute unsigned long long unloadEventEnd;
  readonly attribute unsigned long long redirectStart;
  readonly attribute unsigned long long redirectEnd;
  readonly attribute unsigned long long fetchStart;
  readonly attribute unsigned long long domainLookupStart;
  readonly attribute unsigned long long domainLookupEnd;
  readonly attribute unsigned long long connectStart;
  readonly attribute unsigned long long connectEnd;
  readonly attribute unsigned long long secureConnectionStart;
  readonly attribute unsigned long long requestStart;
  readonly attribute unsigned long long responseStart;
  readonly attribute unsigned long long responseEnd;
  readonly attribute unsigned long long domLoading;
  readonly attribute unsigned long long domInteractive;
  readonly attribute unsigned long long domContentLoadedEventStart;
  readonly attribute unsigned long long domContentLoadedEventEnd;
  readonly attribute unsigned long long domComplete;
  readonly attribute unsigned long long loadEventStart;
  readonly attribute unsigned long long loadEventEnd;
};

interface MediaQueryList : EventTarget {
  readonly attribute DOMString media;
  readonly attribute boolean matches;
  attribute any onchange;
  undefined addListener(any callback);
  undefined removeListener(any callback);
};

interface PluginArray {
  getter any item(unsigned long index);
  any namedItem(DOMString name);
  readonly attribute unsigned long length;
  undefined refresh();
};

interface MimeTypeArray {
  getter any item(unsigned long index);
  any namedItem(DOMString name);
  readonly attribute unsigned long length;
};

partial interface Document {
  readonly attribute any defaultView;
  readonly attribute USVString referrer;
  readonly attribute DOMString readyState;
  attribute any onreadystatechange;
  readonly attribute Element? currentScript;
  [CEReactions] attribute DOMString title;
  readonly attribute HTMLElement? body;
  readonly attribute HTMLElement? head;
  undefined write(DOMString... text);
  undefined writeln(DOMString... text);
};

partial interface Element {
  [CEReactions] attribute DOMString innerHTML;
  [CEReactions] attribute DOMString outerHTML;
  [CEReactions] undefined insertAdjacentHTML(DOMString position, DOMString string);
};
