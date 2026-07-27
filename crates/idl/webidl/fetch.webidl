// Fetch Phase 3 surface. Promise-returning methods are typed `any`
// (the codegen has no Promise type): the hand-written `imp` builds and returns
// the promise value. `fetch()` itself is a global function hand-registered in
// `install_window`, not an interface here.
//
// `XMLHttpRequest` lives in `xhr.webidl`: it is a separate specification with
// its own inheritance chain (`XMLHttpRequestEventTarget`) and its own event
// interface (`ProgressEvent`), and it shares only `FormData`/`Headers` with
// this file.

// The `FormData` entry list. Constructing one from a `<form>` runs HTML's
// "construct the entry list" over the form's successful controls.
//
// Values are `DOMString` only: `Blob`/`File` do not exist in this engine, so
// there is nothing a file entry could hold. Pair iteration (`entries`/`keys`/
// `values`/`forEach`/`@@iterator`) is installed on the prototype from
// `bootstrap.js`, sharing `URLSearchParams`' helper.
interface FormData {
  constructor(optional any form);

  undefined append(USVString name, USVString value);
  undefined delete(USVString name);
  USVString? get(USVString name);
  sequence<USVString> getAll(USVString name);
  boolean has(USVString name);
  undefined set(USVString name, USVString value);
};

interface Headers {
  constructor(optional any init);

  undefined append(USVString name, USVString value);
  undefined delete(USVString name);
  USVString? get(USVString name);
  boolean has(USVString name);
  undefined set(USVString name, USVString value);
  undefined forEach(any callback);
};

interface Request {
  constructor(any input, optional any init);

  readonly attribute USVString method;
  readonly attribute USVString url;
  [SameObject] readonly attribute Headers headers;
  readonly attribute USVString destination;
  readonly attribute USVString referrer;
  readonly attribute USVString referrerPolicy;
  readonly attribute USVString mode;
  readonly attribute USVString credentials;
  readonly attribute USVString cache;
  readonly attribute USVString redirect;
  readonly attribute USVString integrity;
  readonly attribute boolean keepalive;
  readonly attribute boolean bodyUsed;

  any text();
  any json();
  any arrayBuffer();
  any clone();
};

interface Response {
  constructor(optional any body, optional any init);

  readonly attribute USVString type;
  readonly attribute USVString url;
  readonly attribute boolean redirected;
  readonly attribute unsigned short status;
  readonly attribute boolean ok;
  readonly attribute USVString statusText;
  [SameObject] readonly attribute Headers headers;
  readonly attribute boolean bodyUsed;

  any text();
  any json();
  any arrayBuffer();
};
