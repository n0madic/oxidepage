//! File API tests (ADR-0032 Phase 4): `Blob`, `File`, `FileList`, `FileReader`.
//!
//! A separate harness from `bindings.rs` because this one needs a **real task
//! queue**: `FileReader` completes in a queued task rather than inline, and a
//! test that could not run the queue could only ever assert that nothing had
//! happened yet. `schedule_timer` therefore records the callback and
//! [`Harness::run_tasks`] drains it, which is also what lets the tests prove
//! the "not inline" half.

use std::cell::RefCell;
use std::rc::Rc;

use oxidepage_base::{RequestId, id::FIRST_GENERATION};
use oxidepage_bindings::{
    BindCx, ConsoleMessage, DialogRequest, DialogResponse, FileInput, HostHooks, PageState,
    PrivateStorageAreas, ScriptError, SharedStorage, StorageAreaKind, install,
};
use oxidepage_dom::{ParseOptions, parse_document};
use oxidepage_js::{JsEngine, JsRealm, JsValue, QuickJsEngine, RealmOptions};
use oxidepage_net::NetRequest;

#[derive(Default)]
struct TestHooks {
    /// Zero-delay tasks queued through `schedule_timer`, oldest first. The
    /// `FileReader` read path is the only thing that queues here in these
    /// tests, so draining this *is* turning the crank of the event loop.
    tasks: RefCell<Vec<(f64, JsValue)>>,
    next_timer: std::cell::Cell<f64>,
    console: RefCell<Vec<ConsoleMessage>>,
    errors: RefCell<Vec<ScriptError>>,
    storage: PrivateStorageAreas,
    /// Every request handed to the embedder, so a body extracted from a `Blob`
    /// can be asserted on the wire rather than through a JS mirror of it.
    sent: RefCell<Vec<NetRequest>>,
}

impl HostHooks for TestHooks {
    fn storage(&self, kind: StorageAreaKind, origin: &str) -> SharedStorage {
        self.storage.area(kind, origin)
    }

    fn console_message(&self, message: ConsoleMessage) {
        self.console.borrow_mut().push(message);
    }

    fn report_error(&self, error: ScriptError) {
        self.errors.borrow_mut().push(error);
    }

    fn run_dialog(&self, _request: DialogRequest) -> DialogResponse {
        DialogResponse::default()
    }

    fn schedule_timer(
        &self,
        callback: JsValue,
        _args: Vec<JsValue>,
        _delay_ms: f64,
        _repeat: bool,
    ) -> f64 {
        let id = self.next_timer.get() + 1.0;
        self.next_timer.set(id);
        self.tasks.borrow_mut().push((id, callback));
        id
    }

    fn clear_timer(&self, id: f64) {
        self.tasks.borrow_mut().retain(|(queued, _)| *queued != id);
    }

    fn request_animation_frame(&self, _callback: JsValue) -> f64 {
        0.0
    }

    fn cancel_animation_frame(&self, _id: f64) {}

    fn start_fetch(&self, request: NetRequest) -> RequestId {
        self.sent.borrow_mut().push(request);
        RequestId::from_parts(1, FIRST_GENERATION)
    }

    fn abort(&self, _id: RequestId) {}

    fn get_cookie(&self, _document_url: &str) -> String {
        String::new()
    }

    fn set_cookie(&self, _document_url: &str, _cookie: &str) {}
}

struct Harness {
    // Field order = drop order: the state owns persistent JS references and
    // must drop before the realm.
    state: Rc<PageState>,
    hooks: Rc<TestHooks>,
    realm: oxidepage_js::QuickJsRealm,
}

impl Harness {
    fn new() -> Self {
        let realm = QuickJsEngine
            .new_realm(RealmOptions::default())
            .expect("realm");
        let dom = Rc::new(RefCell::new(
            parse_document(
                "<!DOCTYPE html><html><body></body></html>",
                ParseOptions::default(),
            )
            .tree,
        ));
        let hooks = Rc::new(TestHooks::default());
        let state = install(
            &realm,
            dom,
            Rc::clone(&hooks) as Rc<dyn HostHooks>,
            oxidepage_style::Viewport::default(),
        )
        .expect("install");
        Self {
            state,
            hooks,
            realm,
        }
    }

    fn eval(&self, source: &str) -> Result<JsValue, oxidepage_js::JsError> {
        self.realm.with_scope(|scope| {
            let result = scope.eval(source, "test.js");
            let cx = BindCx {
                scope,
                state: Rc::clone(&self.state),
            };
            oxidepage_bindings::microtask_checkpoint(&cx);
            result
        })
    }

    /// Runs every queued task (and the tasks they queue), with a microtask
    /// checkpoint after each — the embedder's loop, minus everything a
    /// `FileReader` cannot reach.
    fn run_tasks(&self) {
        for _ in 0..16 {
            let due: Vec<JsValue> = self
                .hooks
                .tasks
                .borrow_mut()
                .drain(..)
                .map(|(_, cb)| cb)
                .collect();
            if due.is_empty() {
                return;
            }
            self.realm.with_scope(|scope| {
                let cx = BindCx {
                    scope,
                    state: Rc::clone(&self.state),
                };
                for callback in due {
                    scope
                        .call(&callback, &JsValue::Undefined, &[])
                        .expect("queued task");
                    oxidepage_bindings::microtask_checkpoint(&cx);
                }
            });
        }
        panic!("queued tasks never settled");
    }

    /// Installs a `FileList` built from embedder data as a global. There is no
    /// `DataTransfer`, so this is the only way one can exist — which is exactly
    /// the point (ADR-0032 D11).
    fn install_file_list(&self, name: &str, files: Vec<FileInput>) {
        self.realm.with_scope(|scope| {
            let cx = BindCx {
                scope,
                state: Rc::clone(&self.state),
            };
            let list = cx.new_file_list(files).expect("file list");
            let global = scope.global();
            scope.set(&global, name, &list).expect("install global");
        });
    }

    fn string(&self, source: &str) -> String {
        match self.eval(source) {
            Ok(JsValue::String(s)) => s,
            Ok(other) => panic!("expected string from `{source}`, got {other:?}"),
            Err(e) => panic!("eval `{source}` failed: {e}"),
        }
    }

    fn number(&self, source: &str) -> f64 {
        match self.eval(source) {
            Ok(JsValue::Number(n)) => n,
            Ok(other) => panic!("expected number from `{source}`, got {other:?}"),
            Err(e) => panic!("eval `{source}` failed: {e}"),
        }
    }

    fn truthy(&self, source: &str) -> bool {
        match self.eval(source) {
            Ok(JsValue::Bool(b)) => b,
            Ok(other) => panic!("expected bool from `{source}`, got {other:?}"),
            Err(e) => panic!("eval `{source}` failed: {e}"),
        }
    }
}

/// Evaluates `source` and reports the constructor name of whatever it threw,
/// or `"NO THROW"`.
fn threw(h: &Harness, source: &str) -> String {
    h.string(&format!(
        "(() => {{ try {{ {source}; return 'NO THROW'; }} catch (e) {{ return e.constructor.name; }} }})()"
    ))
}

// === Blob ===

#[test]
fn blob_concatenates_its_parts() {
    let h = Harness::new();
    assert_eq!(h.number("new Blob(['a', 'b']).size"), 2.0);
    assert_eq!(h.number("new Blob([]).size"), 0.0);
    assert_eq!(h.number("new Blob().size"), 0.0);
    // A multi-byte string part is UTF-8 encoded, not counted in code units.
    assert_eq!(h.number("new Blob(['é']).size"), 2.0);
    // Any iterable is a part list, and a nested blob contributes its bytes.
    assert_eq!(
        h.number("new Blob([new Blob(['ab']), 'c', new Set(['d']).size]).size"),
        4.0
    );
}

#[test]
fn blob_parts_must_be_iterable() {
    let h = Harness::new();
    // A bare string is the classic mistake; the spec refuses it rather than
    // treating it as a one-element list.
    assert_eq!(threw(&h, "new Blob('abc')"), "TypeError");
    assert_eq!(threw(&h, "new Blob(42)"), "TypeError");
    assert_eq!(threw(&h, "new Blob(null)"), "TypeError");
}

#[test]
fn blob_accepts_buffer_source_parts() {
    let h = Harness::new();
    h.eval("globalThis.buf = new Uint8Array([104, 105]).buffer")
        .unwrap();
    assert_eq!(h.number("new Blob([buf]).size"), 2.0);
    assert_eq!(h.number("new Blob([new Uint8Array(buf)]).size"), 2.0);
    // A view with a non-zero offset contributes only its own window.
    assert_eq!(
        h.number("new Blob([new Uint8Array(new Uint8Array([1,2,3,4]).buffer, 1, 2)]).size"),
        2.0
    );
    h.eval("globalThis.out = null; new Blob([buf]).text().then(t => { globalThis.out = t; })")
        .unwrap();
    assert_eq!(h.string("out"), "hi");
    // High bytes survive the Latin-1 round trip through the bootstrap helper:
    // 0xC3 0xA9 is UTF-8 for `é`.
    h.eval("globalThis.out2 = null; new Blob([new Uint8Array([0xc3, 0xa9])]).text().then(t => { globalThis.out2 = t; })")
        .unwrap();
    assert_eq!(h.string("out2"), "é");
}

#[test]
fn blob_type_is_lowercased_and_non_printable_types_are_rejected() {
    let h = Harness::new();
    assert_eq!(
        h.string("new Blob([], {type: 'TEXT/Plain'}).type"),
        "text/plain"
    );
    assert_eq!(h.string("new Blob([]).type"), "");
    assert_eq!(h.string("new Blob([], {}).type"), "");
    // A control character anywhere rejects the whole value — it would
    // otherwise reach a `Content-Type` header and a `data:` URL.
    assert_eq!(
        h.string("new Blob([], {type: 'text/plain\\r\\nX: y'}).type"),
        ""
    );
    assert_eq!(h.string("new Blob([], {type: 'tëxt/plain'}).type"), "");
}

#[test]
fn blob_slice_clamps_relative_indices() {
    let h = Harness::new();
    h.eval("globalThis.b = new Blob(['0123456789'], {type: 'text/plain'})")
        .unwrap();
    let text = |expr: &str| -> String {
        h.eval(&format!(
            "globalThis.r = null; ({expr}).text().then(t => {{ globalThis.r = t; }})"
        ))
        .unwrap();
        h.string("r")
    };
    assert_eq!(text("b.slice(0, 3)"), "012");
    assert_eq!(text("b.slice(3)"), "3456789");
    assert_eq!(text("b.slice(-3)"), "789");
    assert_eq!(text("b.slice(-3, -1)"), "78");
    assert_eq!(text("b.slice()"), "0123456789");
    // Out of range in both directions, and a reversed range, are all empty
    // rather than an error.
    assert_eq!(text("b.slice(100, 200)"), "");
    assert_eq!(text("b.slice(-100, 2)"), "01");
    assert_eq!(text("b.slice(5, 2)"), "");
    assert_eq!(h.number("b.slice(5, 2).size"), 0.0);
    // A slice of a slice is relative to the slice, not to the original.
    assert_eq!(text("b.slice(2, 8).slice(1, 3)"), "34");
    // The type is *not* inherited; it comes only from the argument.
    assert_eq!(h.string("b.slice(0, 1).type"), "");
    assert_eq!(h.string("b.slice(0, 1, 'TEXT/Html').type"), "text/html");
}

#[test]
fn blob_array_buffer_returns_the_bytes() {
    let h = Harness::new();
    h.eval(
        "globalThis.n = null; new Blob(['abc']).arrayBuffer().then(b => { globalThis.n = new Uint8Array(b).join(','); })",
    )
    .unwrap();
    assert_eq!(h.string("n"), "97,98,99");
}

// === File ===

#[test]
fn file_inherits_blob() {
    let h = Harness::new();
    h.eval("globalThis.f = new File(['hello'], 'a.txt', {type: 'text/plain', lastModified: 1234})")
        .unwrap();
    assert!(h.truthy("f instanceof File"));
    assert!(h.truthy("f instanceof Blob"));
    assert!(h.truthy("Object.getPrototypeOf(File.prototype) === Blob.prototype"));
    assert_eq!(h.string("f.name"), "a.txt");
    assert_eq!(h.number("f.lastModified"), 1234.0);
    // The Blob half is the same implementation, reached through the chain.
    assert_eq!(h.number("f.size"), 5.0);
    assert_eq!(h.string("f.type"), "text/plain");
    // Slicing a file yields bytes, not a differently-named file.
    assert!(h.truthy("f.slice(0, 1) instanceof Blob"));
    assert!(h.truthy("!(f.slice(0, 1) instanceof File)"));
}

#[test]
fn file_name_cannot_carry_a_path_separator() {
    let h = Harness::new();
    assert_eq!(
        h.string("new File([], '../../etc/passwd').name"),
        "..:..:etc:passwd"
    );
}

#[test]
fn file_last_modified_defaults_to_now() {
    let h = Harness::new();
    // No wall-clock assertion beyond "a plausible epoch millisecond": the
    // point is that it is not 0 and not NaN.
    assert!(h.truthy("new File([], 'a').lastModified > 1000000000000"));
}

#[test]
fn a_blob_receiver_is_not_a_file() {
    let h = Harness::new();
    // The `File` accessors brand-check beyond `Blob`-ness, so they cannot
    // report a name a plain blob never had.
    assert_eq!(
        threw(
            &h,
            "Object.getOwnPropertyDescriptor(File.prototype, 'name').get.call(new Blob([]))"
        ),
        "TypeError"
    );
}

// === FileList ===

fn one_file(name: &str, bytes: &[u8]) -> FileInput {
    FileInput {
        name: name.to_owned(),
        bytes: std::rc::Rc::new(bytes.to_vec()),
        content_type: "text/plain".to_owned(),
        last_modified: 5,
    }
}

#[test]
fn file_list_is_indexed_and_iterable() {
    let h = Harness::new();
    h.install_file_list(
        "files",
        vec![one_file("a.txt", b"aa"), one_file("b.txt", b"b")],
    );
    assert_eq!(h.number("files.length"), 2.0);
    assert_eq!(h.string("files[0].name"), "a.txt");
    assert_eq!(h.string("files.item(1).name"), "b.txt");
    assert!(h.truthy("files[0] instanceof File"));
    assert_eq!(h.number("files[0].size"), 2.0);
    assert!(h.truthy("files.item(2) === null"));
    assert!(h.truthy("files[2] === undefined"));
    // ADR-0031 D6's lesson: an indexed-getter interface missing from
    // `install_value_iterators` makes this throw for no reason a page author
    // can see.
    assert_eq!(
        h.string("[...files].map(f => f.name).join(',')"),
        "a.txt,b.txt"
    );
    assert_eq!(
        h.string("Array.from(files).map(f => f.name).join(',')"),
        "a.txt,b.txt"
    );
    assert!(h.truthy("FileList.prototype[Symbol.iterator] === Array.prototype.values"));
}

#[test]
fn a_file_list_cannot_be_constructed_from_script() {
    let h = Harness::new();
    assert_eq!(threw(&h, "new FileList()"), "TypeError");
}

// === FileReader ===

/// Records every event a reader fires, in order, with the result visible at
/// the moment it fired — which is what pins `load` before `loadend` and
/// `result` being set by the time `load` runs.
const RECORDER: &str = r#"
globalThis.log = [];
globalThis.r = new FileReader();
for (const type of ['loadstart', 'progress', 'load', 'error', 'abort', 'loadend']) {
    r.addEventListener(type, (e) => {
        log.push(type + ':' + r.readyState + ':' + (typeof r.result));
    });
}
"#;

#[test]
fn read_as_text_completes_in_a_task_not_inline() {
    let h = Harness::new();
    h.eval(RECORDER).unwrap();
    h.eval("r.readAsText(new Blob(['hi']))").unwrap();
    // Nothing at all has fired yet: not even `loadstart`.
    assert_eq!(h.string("log.join('|')"), "");
    assert_eq!(h.number("r.readyState"), 1.0);
    assert!(h.truthy("r.result === null"));

    h.run_tasks();
    assert_eq!(
        h.string("log.join('|')"),
        "loadstart:1:object|load:2:string|loadend:2:string"
    );
    assert_eq!(h.string("r.result"), "hi");
    assert_eq!(h.number("r.readyState"), 2.0);
    assert!(h.truthy("r.error === null"));
}

#[test]
fn the_on_handlers_are_the_same_registry_as_the_listeners() {
    let h = Harness::new();
    h.eval(
        "globalThis.seen = []; globalThis.r = new FileReader();
         r.onload = (e) => seen.push('onload:' + e.type + ':' + (e instanceof ProgressEvent) + ':' + (e.target === r));
         r.onloadend = () => seen.push('onloadend');
         r.readAsText(new Blob(['x']));",
    )
    .unwrap();
    assert!(h.truthy("typeof r.onload === 'function'"));
    h.run_tasks();
    assert_eq!(
        h.string("seen.join('|')"),
        "onload:load:true:true|onloadend"
    );
    // Assigning a non-function removes the handler, per the IDL.
    h.eval("r.onload = null").unwrap();
    assert!(h.truthy("r.onload === null"));
}

#[test]
fn read_as_text_honours_an_explicit_encoding() {
    let h = Harness::new();
    h.eval(
        "globalThis.r = new FileReader();
         r.readAsText(new Blob([new Uint8Array([0xe9])]), 'windows-1252');",
    )
    .unwrap();
    h.run_tasks();
    assert_eq!(h.string("r.result"), "é");

    // With no argument the blob's own `charset=` wins, and UTF-8 is the last
    // resort.
    let h = Harness::new();
    h.eval(
        "globalThis.r = new FileReader();
         r.readAsText(new Blob([new Uint8Array([0xe9])], {type: 'text/plain;charset=windows-1252'}));",
    )
    .unwrap();
    h.run_tasks();
    assert_eq!(h.string("r.result"), "é");
}

#[test]
fn read_as_data_url_is_base64_with_the_blob_type() {
    let h = Harness::new();
    h.eval(
        "globalThis.r = new FileReader(); r.readAsDataURL(new Blob(['hi'], {type: 'text/plain'}))",
    )
    .unwrap();
    h.run_tasks();
    assert_eq!(h.string("r.result"), "data:text/plain;base64,aGk=");

    // A typeless blob still produces a well-formed data URL.
    let h = Harness::new();
    h.eval("globalThis.r = new FileReader(); r.readAsDataURL(new Blob(['foobar']))")
        .unwrap();
    h.run_tasks();
    assert_eq!(h.string("r.result"), "data:;base64,Zm9vYmFy");
}

#[test]
fn read_as_array_buffer_yields_an_array_buffer() {
    let h = Harness::new();
    h.eval("globalThis.r = new FileReader(); r.readAsArrayBuffer(new Blob(['abc']))")
        .unwrap();
    h.run_tasks();
    assert!(h.truthy("r.result instanceof ArrayBuffer"));
    assert_eq!(h.string("new Uint8Array(r.result).join(',')"), "97,98,99");
}

#[test]
fn a_concurrent_read_is_an_invalid_state_error() {
    let h = Harness::new();
    h.eval("globalThis.r = new FileReader(); r.readAsText(new Blob(['a']))")
        .unwrap();
    assert_eq!(threw(&h, "r.readAsText(new Blob(['b']))"), "DOMException");
    h.run_tasks();
    // The first read still completes; the refused second one changed nothing.
    assert_eq!(h.string("r.result"), "a");
    // Once DONE, the reader is reusable.
    h.eval("r.readAsText(new Blob(['b']))").unwrap();
    h.run_tasks();
    assert_eq!(h.string("r.result"), "b");
}

#[test]
fn abort_cancels_the_queued_completion() {
    let h = Harness::new();
    h.eval(RECORDER).unwrap();
    h.eval("r.readAsText(new Blob(['hi'])); r.abort();")
        .unwrap();
    // `abort` and `loadend` fire synchronously from `abort()`, as the spec has
    // them; `loadstart` never gets a chance to.
    assert_eq!(h.string("log.join('|')"), "abort:2:object|loadend:2:object");
    assert!(h.truthy("r.result === null"));
    h.run_tasks();
    // The already-queued tasks find a stale token and do nothing.
    assert_eq!(h.string("log.join('|')"), "abort:2:object|loadend:2:object");
    assert!(h.truthy("r.result === null"));
    assert_eq!(h.number("r.readyState"), 2.0);
}

#[test]
fn abort_outside_a_read_fires_nothing() {
    let h = Harness::new();
    h.eval(RECORDER).unwrap();
    h.eval("r.abort()").unwrap();
    assert_eq!(h.string("log.join('|')"), "");
    assert_eq!(h.number("r.readyState"), 0.0);
}

#[test]
fn file_reader_is_an_event_target_with_the_idl_constants() {
    let h = Harness::new();
    assert!(h.truthy("new FileReader() instanceof EventTarget"));
    assert_eq!(h.number("FileReader.EMPTY"), 0.0);
    assert_eq!(h.number("FileReader.LOADING"), 1.0);
    assert_eq!(h.number("FileReader.DONE"), 2.0);
    assert_eq!(h.number("FileReader.prototype.DONE"), 2.0);
    // P6: `readAsBinaryString` is not implemented, so it does not exist.
    assert!(h.truthy("!('readAsBinaryString' in FileReader.prototype)"));
    // A non-Blob argument is a TypeError, not a silent no-op.
    assert_eq!(
        threw(&h, "new FileReader().readAsText('nope')"),
        "TypeError"
    );
}

#[test]
fn a_file_reads_like_any_other_blob() {
    let h = Harness::new();
    h.eval(
        "globalThis.r = new FileReader();
         r.readAsText(new File(['contents'], 'note.txt', {type: 'text/plain'}));",
    )
    .unwrap();
    h.run_tasks();
    assert_eq!(h.string("r.result"), "contents");
}

// === The interfaces Blob unblocked ===

#[test]
fn response_blob_carries_the_content_type() {
    let h = Harness::new();
    h.eval(
        "globalThis.b = null;
         new Response('hello', {headers: {'Content-Type': 'TEXT/Plain'}})
            .blob().then(v => { globalThis.b = v; });",
    )
    .unwrap();
    assert!(h.truthy("b instanceof Blob"));
    assert_eq!(h.number("b.size"), 5.0);
    assert_eq!(h.string("b.type"), "text/plain");
    // The body is single-use, exactly as `text()`/`arrayBuffer()` are.
    assert!(h.truthy("new Response('x').bodyUsed === false"));
}

#[test]
fn xhr_accepts_the_blob_response_type() {
    let h = Harness::new();
    h.eval("globalThis.x = new XMLHttpRequest(); x.open('GET', 'https://example.com/');")
        .unwrap();
    h.eval("x.responseType = 'blob'").unwrap();
    assert_eq!(h.string("x.responseType"), "blob");
    // An unsupported value is still ignored rather than installed.
    h.eval("x.responseType = 'nonsense'").unwrap();
    assert_eq!(h.string("x.responseType"), "blob");
}

#[test]
fn a_blob_is_a_request_body() {
    // Asserted against the `NetRequest` that reaches the embedder rather than
    // against a JS object: `body::extract` produces bytes *and* a default
    // `Content-Type`, and the wire is the only place both are visible together.
    let h = Harness::new();
    h.eval(
        "const x = new XMLHttpRequest();
         x.open('POST', 'https://example.com/');
         x.send(new Blob(['ab'], {type: 'TEXT/Plain'}));",
    )
    .unwrap();
    let sent = h.hooks.sent.borrow().last().cloned().expect("a request");
    assert_eq!(sent.body.as_deref(), Some(&b"ab"[..]));
    assert_eq!(content_type(&sent).as_deref(), Some("text/plain"));

    // A typeless blob leaves the header absent rather than sending an empty
    // one.
    let h = Harness::new();
    h.eval(
        "const x = new XMLHttpRequest();
         x.open('POST', 'https://example.com/');
         x.send(new Blob(['ab']));",
    )
    .unwrap();
    let sent = h.hooks.sent.borrow().last().cloned().expect("a request");
    assert_eq!(sent.body.as_deref(), Some(&b"ab"[..]));
    assert_eq!(content_type(&sent), None);

    // An author-set header still wins over the blob's type.
    let h = Harness::new();
    h.eval(
        "const x = new XMLHttpRequest();
         x.open('POST', 'https://example.com/');
         x.setRequestHeader('Content-Type', 'application/json');
         x.send(new Blob(['{}'], {type: 'text/plain'}));",
    )
    .unwrap();
    let sent = h.hooks.sent.borrow().last().cloned().expect("a request");
    assert_eq!(content_type(&sent).as_deref(), Some("application/json"));
}

#[test]
fn a_file_is_a_request_body_too() {
    let h = Harness::new();
    h.eval(
        "const x = new XMLHttpRequest();
         x.open('POST', 'https://example.com/');
         x.send(new File(['note'], 'a.txt', {type: 'text/plain'}));",
    )
    .unwrap();
    let sent = h.hooks.sent.borrow().last().cloned().expect("a request");
    assert_eq!(sent.body.as_deref(), Some(&b"note"[..]));
    assert_eq!(content_type(&sent).as_deref(), Some("text/plain"));
}

/// Iterating a `FormData` yields the same values `get()` does.
///
/// The serializers flatten a file entry to its filename — right for the two
/// encodings that cannot carry bytes, wrong for the iterator. When `get()`
/// returned a `File` and `[...fd]` returned `"photo.png"`, the two accessors
/// disagreed about one entry, and the
/// `for (const [k, v] of fd) if (v instanceof File)` idiom — how every upload
/// library detects file parts — silently took the wrong branch.
#[test]
fn iterating_a_form_data_agrees_with_get() {
    let h = Harness::new();
    h.eval(
        "const fd = new FormData();
         fd.append('text', 'plain');
         fd.append('photo', new File(['png'], 'photo.png', {type: 'image/png'}), 'photo.png');

         const viaGet = fd.get('photo');
         const viaIter = [...fd].find(([k]) => k === 'photo')[1];
         const viaEntries = [...fd.entries()].find(([k]) => k === 'photo')[1];
         let viaForEach;
         fd.forEach((v, k) => { if (k === 'photo') viaForEach = v; });

         globalThis.out = JSON.stringify([
             viaGet instanceof File,
             viaIter instanceof File,
             viaEntries instanceof File,
             viaForEach instanceof File,
             viaIter.name,
             viaIter.size,
             // A text entry is still a plain string everywhere.
             typeof fd.get('text'),
             typeof [...fd].find(([k]) => k === 'text')[1],
         ]);",
    )
    .unwrap();
    assert_eq!(
        h.string("out"),
        r#"[true,true,true,true,"photo.png",3,"string","string"]"#
    );
}

/// `new Request(url, { body })` goes through the same extractor as `fetch()` and
/// `xhr.send()`.
///
/// It used to `coerce_string` the value, so a `Blob` body reached the wire as
/// the literal text `[object Blob]` and a `FormData` as `[object FormData]` —
/// silent, and indistinguishable from a server bug. Asserted on the real
/// `NetRequest`, because a string body and a byte body look identical from JS.
#[test]
fn a_request_body_is_extracted_not_stringified() {
    // Read back through `fetch(request)`, which is the only consumer that
    // actually sends a `Request`'s body.
    let h = Harness::new();
    h.eval(
        "globalThis.__r = new Request('https://example.com/', {
             method: 'POST',
             body: new Blob(['ab'], {type: 'text/plain'}),
         });
         fetch(globalThis.__r);",
    )
    .unwrap();
    let sent = h.hooks.sent.borrow().last().cloned().expect("a request");
    assert_eq!(
        sent.body.as_deref(),
        Some(&b"ab"[..]),
        "a Blob body must reach the wire as its bytes, not as `[object Blob]`"
    );
    assert_eq!(content_type(&sent).as_deref(), Some("text/plain"));

    // `FormData` is the case this stage made load-bearing: the multipart
    // boundary exists nowhere but the extracted `Content-Type`.
    let h = Harness::new();
    h.eval(
        "const f = new FormData();
         f.append('a', '1');
         globalThis.__r = new Request('https://example.com/', { method: 'POST', body: f });
         fetch(globalThis.__r);",
    )
    .unwrap();
    let sent = h.hooks.sent.borrow().last().cloned().expect("a request");
    let ct = content_type(&sent).expect("a content type");
    assert!(
        ct.starts_with("multipart/form-data; boundary="),
        "got `{ct}`"
    );
    let boundary = ct.split_once("boundary=").expect("a boundary").1;
    let body = String::from_utf8(sent.body.clone().expect("a body")).expect("utf-8");
    assert!(
        body.contains(boundary) && body.contains("name=\"a\""),
        "the body must be real multipart naming the same boundary: {body}"
    );
}

fn content_type(request: &NetRequest) -> Option<String> {
    request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone())
}
