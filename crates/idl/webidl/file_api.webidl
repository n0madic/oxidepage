// File API (https://w3c.github.io/FileAPI/), the ADR-0032 Phase 4 surface:
// the two byte-carrying interfaces, the list an `<input type=file>` exposes,
// and the reader that hands their bytes to script.
//
// One backing `BlobData` serves both interfaces (ADR-0032 D10): it owns an
// `Rc<Vec<u8>>` plus a `[start, end)` window, so `slice()` is a *view* and
// never copies, and the `File` half is an optional metadata record. That is
// why `File` inherits `Blob` here rather than duplicating its members —
// `this_blob` accepts either, `this_file` demands the metadata.
//
// Three signatures are hand-marshalled because the codegen rejects the types
// the spec uses, and it rejects them at *build* time rather than silently:
//
// - `sequence<BlobPart>` as an **argument** is unsupported, and `BlobPart` is
//   a union of `BufferSource`/`Blob`/`USVString` besides. So `parts` and
//   `fileBits` are `any`, and `imp::blob::parts_of` walks the iterable.
// - `ArrayBuffer`/`ArrayBufferView` are unsupported as any kind of type, so
//   `readAsArrayBuffer`'s *result* travels through the `any`-typed `result`
//   attribute, and buffer *arguments* are read through the `blobPartBytes`
//   bootstrap helper.
// - There is no `Promise<T>`, so `text()` and `arrayBuffer()` are `any` and
//   the imp returns a finished promise — the convention `fetch.webidl`
//   already uses.
//
// `slice`'s `long long` arguments *are* declared as the spec has them: the
// codegen grew `ArgKind::I64` for exactly this member, because clamping a
// blob offset through `long` would silently mis-slice a >2 GiB blob rather
// than fail. The spec's `[Clamp]` is applied inside `imp::blob::slice`, where
// the relative-index algorithm lives anyway.

interface Blob {
  constructor(optional any parts, optional any options);

  readonly attribute unsigned long long size;
  readonly attribute DOMString type;

  Blob slice(optional long long start, optional long long end,
             optional DOMString contentType);

  any text();
  any arrayBuffer();
};

interface File : Blob {
  constructor(any fileBits, DOMString fileName, optional any options);

  readonly attribute DOMString name;
  readonly attribute long long lastModified;
};

// The `DOMRectList` shape: an indexed getter plus `length`, wrapped in the
// collection proxy so `files[0]` works, and registered in
// `install_value_iterators` so `[...input.files]` does too (ADR-0031 D6).
//
// There is no `DataTransfer` in this engine, so a `FileList` is only ever
// minted by the embedder (`DOM.setFileInputFiles`, the file chooser) — never
// by page script.
interface FileList {
  readonly attribute unsigned long length;
  getter File? item(unsigned long index);
};

// A real `EventTarget`, for the same reason `XMLHttpRequest` is one: the
// events it fires are genuine `ProgressEvent`s dispatched through the shared
// registry, so listener options, `event.target` and `instanceof` all work.
//
// The `onX` handlers are typed `any` rather than `EventHandler` for the reason
// `xhr.webidl` records at length: an `EventHandler` attribute also joins
// `EVENT_HANDLER_TYPES`, the list of event-handler *content* attributes, and
// these are not that.
//
// `readAsBinaryString` is deliberately absent (ADR-0032's limits): it is
// deprecated, and P6 says an unimplemented API must not exist rather than
// exist and lie. `result` and `error` are `any` because they hold, variously,
// a string, an `ArrayBuffer`, `null`, and a `DOMException`.
interface FileReader : EventTarget {
  constructor();

  const unsigned short EMPTY = 0;
  const unsigned short LOADING = 1;
  const unsigned short DONE = 2;

  // `Blob` is a pass-through type, so these arrive as raw values and
  // `imp::file_reader::start` brand-checks them — a non-`Blob` argument is the
  // `TypeError` WebIDL would have raised.
  undefined readAsText(Blob blob, optional DOMString encoding);
  undefined readAsDataURL(Blob blob);
  undefined readAsArrayBuffer(Blob blob);
  undefined abort();

  readonly attribute unsigned short readyState;
  readonly attribute any result;
  readonly attribute any error;

  attribute any onloadstart;
  attribute any onprogress;
  attribute any onload;
  attribute any onabort;
  attribute any onerror;
  attribute any onloadend;
};
