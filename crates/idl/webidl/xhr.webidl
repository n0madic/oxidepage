// XMLHttpRequest (https://xhr.spec.whatwg.org/) and its `ProgressEvent`.
//
// `ProgressEvent` is *not* a UI event, so it lives here rather than in
// `uievents.webidl` — but it does reuse the shared `EventData::ui` payload slot
// (`UiKind::Progress`), which is what gives its three getters the same
// payload-shape brand check every other event interface gets (ADR-0024).
//
// The spec hierarchy is `EventTarget` <- `XMLHttpRequestEventTarget` <-
// {`XMLHttpRequest`, `XMLHttpRequestUpload`}, and it is declared in full: the
// seven shared handlers belong to the base, so `xhr.upload.onprogress` and
// `xhr.onprogress` are the same member on two objects.
//
// The `onX` handlers are typed `any` rather than `EventHandler` deliberately.
// An `EventHandler` attribute joins `EVENT_HANDLER_TYPES`, which is also the
// list of event-handler *content* attributes — and `<div ontimeout="…">` /
// `<div onreadystatechange="…">` are not handlers in HTML. The accessors are
// hand-written instead, over the same `event_handlers` registry.

dictionary ProgressEventInit : EventInit {
  boolean lengthComputable = false;
  unsigned long long loaded = 0;
  unsigned long long total = 0;
};

interface ProgressEvent : Event {
  constructor(DOMString type, optional ProgressEventInit eventInitDict = {});
  readonly attribute boolean lengthComputable;
  readonly attribute unsigned long long loaded;
  readonly attribute unsigned long long total;
};

interface XMLHttpRequestEventTarget : EventTarget {
  attribute any onloadstart;
  attribute any onprogress;
  attribute any onabort;
  attribute any onerror;
  attribute any onload;
  attribute any ontimeout;
  attribute any onloadend;
};

// The upload object. It has no members of its own — its whole purpose is a
// second event-target identity, so `xhr.upload.onprogress` is distinct from
// `xhr.onprogress`.
interface XMLHttpRequestUpload : XMLHttpRequestEventTarget {};

// A real `EventTarget`, so `addEventListener` gets capture/`once`/`passive`,
// `===` dedup and `handleEvent` objects, `dispatchEvent` exists, and the events
// it fires are real `Event`/`ProgressEvent` objects rather than
// `{type, target}` stand-ins.
//
// `open()` is declared once with the long argument list rather than as the
// spec's two overloads (the generator has no overload resolution); a two-argument
// call takes the declared defaults, which is exactly what the short overload
// says. `async = false` is rejected at runtime — see ADR-0024.
interface XMLHttpRequest : XMLHttpRequestEventTarget {
  constructor();

  const unsigned short UNSENT = 0;
  const unsigned short OPENED = 1;
  const unsigned short HEADERS_RECEIVED = 2;
  const unsigned short LOADING = 3;
  const unsigned short DONE = 4;

  attribute any onreadystatechange;

  readonly attribute unsigned short readyState;

  // Request
  undefined open(USVString method, USVString url, optional boolean async = true,
                 optional USVString? username = null,
                 optional USVString? password = null);
  undefined setRequestHeader(USVString name, USVString value);
  attribute unsigned long timeout;
  attribute boolean withCredentials;
  [SameObject] readonly attribute XMLHttpRequestUpload upload;
  undefined send(optional any body);
  undefined abort();

  // Response
  readonly attribute USVString responseURL;
  readonly attribute unsigned short status;
  readonly attribute USVString statusText;
  USVString? getResponseHeader(USVString name);
  USVString getAllResponseHeaders();
  undefined overrideMimeType(DOMString mime);
  attribute USVString responseType;
  readonly attribute any response;
  readonly attribute USVString responseText;
  readonly attribute Document? responseXML;
};
