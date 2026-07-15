// URL Standard (https://url.spec.whatwg.org/), Phase 3 surface. Backed by the
// `url` crate. `searchParams` is typed as a pass-through interface (Raw); the
// static `URL.parse`/`URL.canParse` are hand-registered in `install_url`.

interface URL {
  constructor(USVString url, optional USVString base);

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
  [SameObject] readonly attribute URLSearchParams searchParams;
  attribute USVString hash;

  USVString toJSON();
};

interface URLSearchParams {
  constructor(optional any init = "");

  readonly attribute unsigned long size;
  undefined append(USVString name, USVString value);
  undefined delete(USVString name, optional USVString value);
  USVString? get(USVString name);
  sequence<USVString> getAll(USVString name);
  boolean has(USVString name, optional USVString value);
  undefined set(USVString name, USVString value);
  undefined sort();
  USVString toString();
};
