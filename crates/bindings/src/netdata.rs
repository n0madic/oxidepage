//! Rust-side data behind the Phase 3 network interfaces (URL, URLSearchParams,
//! Headers, Response, XMLHttpRequest) and the in-flight fetch/XHR bookkeeping.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxidepage_base::{NodeId, RequestId};
use oxidepage_js::{JsThrow, JsValue};
use oxidepage_net::ResponseType;
use url::Url;

use crate::filedata::BlobData;

/// Whether `b` is an RFC 7230 token character (the set allowed in a header
/// field name).
fn is_token_byte(b: u8) -> bool {
    matches!(b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
        | b'^' | b'_' | b'`' | b'|' | b'~'
        | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
}

/// A valid header field name is a non-empty RFC 7230 token.
pub(crate) fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(is_token_byte)
}

/// A valid header value carries no NUL, CR, or LF (blocking response-splitting
/// / header-injection via interior control characters).
pub(crate) fn is_valid_header_value(value: &str) -> bool {
    !value.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n'))
}

/// Rejects an invalid (`name`, `value`) with a `TypeError`, per the Fetch
/// "Headers" API.
fn validate_header(name: &str, value: &str) -> Result<(), JsThrow> {
    if !is_valid_header_name(name) {
        return Err(JsThrow::Type(format!("Invalid header name: {name:?}")));
    }
    if !is_valid_header_value(value) {
        return Err(JsThrow::Type(format!("Invalid value for header {name:?}")));
    }
    Ok(())
}

/// A `Headers` object: an ordered multimap with lowercased names.
#[derive(Default)]
pub(crate) struct HeadersData {
    pub entries: Vec<(String, String)>,
}

impl HeadersData {
    /// Builds from already-decoded pairs (network responses), dropping any
    /// entry with an invalid name/value as a defensive measure — script-facing
    /// insertion goes through [`HeadersData::append`], which rejects instead.
    pub fn from_pairs(pairs: &[(String, String)]) -> Self {
        Self {
            entries: pairs
                .iter()
                .filter(|(n, v)| is_valid_header_name(n) && is_valid_header_value(v))
                .map(|(n, v)| (n.to_ascii_lowercase(), v.clone()))
                .collect(),
        }
    }

    pub fn append(&mut self, name: &str, value: &str) -> Result<(), JsThrow> {
        let value = value.trim();
        validate_header(name, value)?;
        self.entries
            .push((name.to_ascii_lowercase(), value.to_owned()));
        Ok(())
    }

    pub fn set(&mut self, name: &str, value: &str) -> Result<(), JsThrow> {
        let value = value.trim();
        validate_header(name, value)?;
        let name = name.to_ascii_lowercase();
        self.entries.retain(|(n, _)| n != &name);
        self.entries.push((name, value.to_owned()));
        Ok(())
    }

    pub fn delete(&mut self, name: &str) {
        let name = name.to_ascii_lowercase();
        self.entries.retain(|(n, _)| n != &name);
    }

    pub fn get(&self, name: &str) -> Option<String> {
        let name = name.to_ascii_lowercase();
        let values: Vec<&str> = self
            .entries
            .iter()
            .filter(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
            .collect();
        if values.is_empty() {
            None
        } else {
            Some(values.join(", "))
        }
    }

    pub fn has(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        self.entries.iter().any(|(n, _)| *n == name)
    }

    /// Sorted, combined `(name, value)` pairs — the `forEach` iteration order.
    pub fn sorted_combined(&self) -> Vec<(String, String)> {
        let mut names: Vec<String> = self.entries.iter().map(|(n, _)| n.clone()).collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                let value = self.get(&name).unwrap_or_default();
                (name, value)
            })
            .collect()
    }
}

/// A `URL` object: the parsed URL cell plus its `[SameObject]` `searchParams`
/// wrapper, cached on first access. `Deref` exposes the inner `RefCell<Url>` so
/// the accessors read/write it as before.
pub(crate) struct UrlData {
    pub url: Rc<RefCell<Url>>,
    pub search_params: RefCell<Option<JsValue>>,
}

impl UrlData {
    pub fn new(url: Url) -> Self {
        Self {
            url: Rc::new(RefCell::new(url)),
            search_params: RefCell::new(None),
        }
    }
}

impl std::ops::Deref for UrlData {
    type Target = RefCell<Url>;
    fn deref(&self) -> &Self::Target {
        &self.url
    }
}

/// One `FormData` entry value: a string, or a file (ADR-0032 D11).
///
/// `Clone` is cheap for both: a file entry clones an `Rc` and a filename, never
/// the bytes.
#[derive(Clone)]
pub(crate) enum FormDataValue {
    Text(String),
    /// A `Blob` or `File` entry. The bytes are shared with the object the entry
    /// came from — `BlobData` is a view, so appending a 100 MB file to a form
    /// copies nothing.
    File {
        data: Rc<BlobData>,
        /// The `filename` parameter. A bare `Blob` has no name, and the Fetch
        /// spec's entry-creation steps give it the literal `"blob"`.
        filename: String,
    },
}

impl FormDataValue {
    /// The entry as the *string* accessors see it.
    ///
    /// `get()` on a file entry returns the `File` object, not this — but the
    /// urlencoded and `text/plain` serializers, which cannot carry bytes, use
    /// the filename, which is what a browser sends for a file in those
    /// encodings.
    pub fn as_text(&self) -> String {
        match self {
            FormDataValue::Text(text) => text.clone(),
            FormDataValue::File { filename, .. } => filename.clone(),
        }
    }

    #[must_use]
    pub fn is_file(&self) -> bool {
        matches!(self, FormDataValue::File { .. })
    }
}

/// A `FormData` entry list (the "entry list" of the XHR spec).
///
/// Entry order is preserved, which is what every serialization relies on.
pub(crate) struct FormDataData {
    pub list: RefCell<Vec<(String, FormDataValue)>>,
}

impl FormDataData {
    pub fn new(list: Vec<(String, FormDataValue)>) -> Self {
        Self {
            list: RefCell::new(list),
        }
    }

    /// Every entry as a name/string pair — for the two serializations that
    /// cannot carry bytes, and for the pair iterator.
    pub fn pairs(&self) -> Vec<(String, String)> {
        self.list
            .borrow()
            .iter()
            .map(|(name, value)| (name.clone(), value.as_text()))
            .collect()
    }

    /// Whether any entry is a file, which is what forces a form to
    /// `multipart/form-data`.
    pub fn has_file(&self) -> bool {
        self.list.borrow().iter().any(|(_, value)| value.is_file())
    }

    /// Serializes as `multipart/form-data` with the given boundary — the wire
    /// format a `FormData` body is sent in, and the reason a request carrying
    /// one must not have its `Content-Type` set by the caller (the header has to
    /// name the boundary, which only this code knows).
    pub fn to_multipart(&self, boundary: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, value) in self.list.borrow().iter() {
            out.extend_from_slice(b"--");
            out.extend_from_slice(boundary.as_bytes());
            out.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
            out.extend_from_slice(escape_multipart_name(name).as_bytes());
            out.push(b'"');
            match value {
                FormDataValue::Text(text) => {
                    out.extend_from_slice(b"\r\n\r\n");
                    out.extend_from_slice(text.as_bytes());
                }
                FormDataValue::File { data, filename } => {
                    // The filename is escaped with the *same* rule as the field
                    // name, and for the same reason: it is attacker-influenced
                    // — it comes from a `Content-Disposition` a server sent, or
                    // from a name page script chose — and a raw CR/LF in it
                    // forges a header just as effectively.
                    out.extend_from_slice(b"; filename=\"");
                    out.extend_from_slice(escape_multipart_name(filename).as_bytes());
                    out.extend_from_slice(b"\"\r\nContent-Type: ");
                    // Per the Fetch multipart serializer, an entry with no type
                    // is sent as `application/octet-stream`.
                    let content_type = if data.type_.is_empty() {
                        "application/octet-stream"
                    } else {
                        &data.type_
                    };
                    out.extend_from_slice(escape_multipart_name(content_type).as_bytes());
                    out.extend_from_slice(b"\r\n\r\n");
                    out.extend_from_slice(data.view());
                }
            }
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"--\r\n");
        out
    }
}

/// Per the Fetch spec's multipart serializer: a value may not carry a raw CR,
/// LF or `"` into the `Content-Disposition` header, so those three are escaped
/// rather than allowed to forge a header or terminate the field early.
///
/// Applied to the **filename and the content type** as well as the field name
/// (ADR-0032 D11): all three are attacker-influenced and all three land in a
/// header.
fn escape_multipart_name(name: &str) -> String {
    name.replace('\r', "%0D")
        .replace('\n', "%0A")
        .replace('"', "%22")
}

/// A `URLSearchParams` object. When bound to a `URL` it reads/writes that
/// URL's query (a live view); otherwise it owns its own list.
pub(crate) struct UrlSearchParamsData {
    pub url: Option<Rc<RefCell<Url>>>,
    pub list: RefCell<Vec<(String, String)>>,
}

impl UrlSearchParamsData {
    pub fn standalone(list: Vec<(String, String)>) -> Self {
        Self {
            url: None,
            list: RefCell::new(list),
        }
    }

    pub fn bound(url: Rc<RefCell<Url>>) -> Self {
        Self {
            url: Some(url),
            list: RefCell::new(Vec::new()),
        }
    }

    pub fn pairs(&self) -> Vec<(String, String)> {
        match &self.url {
            Some(u) => u
                .borrow()
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
            None => self.list.borrow().clone(),
        }
    }

    pub fn set_pairs(&self, pairs: Vec<(String, String)>) {
        match &self.url {
            Some(u) => {
                let mut url = u.borrow_mut();
                if pairs.is_empty() {
                    url.set_query(None);
                } else {
                    url.query_pairs_mut()
                        .clear()
                        .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
                }
            }
            None => *self.list.borrow_mut() = pairs,
        }
    }

    pub fn serialize(&self) -> String {
        let pairs = self.pairs();
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &pairs {
            ser.append_pair(k, v);
        }
        ser.finish()
    }
}

/// A `Request` object (immutable head + fully-buffered body). Enum-valued
/// request members (`mode`, `credentials`, `cache`, `redirect`) are kept in
/// their serialized string form so the accessors return them verbatim; `fetch`
/// maps `mode`/`credentials` back onto the net layer's enums.
pub(crate) struct RequestData {
    pub method: String,
    pub url: String,
    pub headers: Rc<RefCell<HeadersData>>,
    pub destination: String,
    pub referrer: String,
    pub referrer_policy: String,
    pub mode: String,
    pub credentials: String,
    pub cache: String,
    pub redirect: String,
    pub integrity: String,
    pub keepalive: bool,
    pub body: Option<Vec<u8>>,
    pub body_used: Cell<bool>,
}

/// A `Response` object (immutable head + fully-buffered body).
pub(crate) struct ResponseData {
    pub status: u16,
    pub status_text: String,
    pub url: String,
    pub redirected: bool,
    /// `basic` | `cors` | `opaque` | `default` | `error`.
    pub resp_type: String,
    pub headers: Rc<RefCell<HeadersData>>,
    pub body: Vec<u8>,
    pub body_used: Cell<bool>,
}

/// An `XMLHttpRequest` object. All fields default to the UNSENT/empty state.
///
/// The spec's internal flags are modelled explicitly ([`XhrData::send_flag`],
/// [`XhrData::upload_complete`], the response object cache) rather than
/// inferred from `ready_state`, because they are what the state transitions are
/// actually written against — `send()` twice is an `InvalidStateError` because
/// the send() flag is set, not because of the readyState.
#[derive(Default)]
pub(crate) struct XhrData {
    pub ready_state: u16,
    pub method: String,
    /// The request URL, already absolute: `open()` parses it, as the spec does,
    /// so a bad URL throws there rather than at `send()`.
    pub url: String,
    /// Spec "send() flag": set by `send()`, cleared by `open()` and by every
    /// terminal transition. It, not `readyState`, is what makes a second
    /// `send()` an `InvalidStateError`.
    pub send_flag: bool,
    /// Spec "upload complete flag". Set at `send()` when there is no request
    /// body, otherwise when the response head proves the body went out.
    pub upload_complete: bool,
    /// `withCredentials`: send/store cookies on cross-origin requests.
    pub with_credentials: bool,
    /// `timeout` in milliseconds; 0 means no timeout.
    pub timeout: u32,
    /// The page-side timer arming [`XhrData::timeout`], and the moment `send()`
    /// started — the timeout is measured from `send()`, so re-assigning
    /// `timeout` mid-flight re-arms against the original start.
    pub timeout_timer: Option<f64>,
    pub send_started_ms: f64,
    pub request_headers: Vec<(String, String)>,
    pub response_type: String,
    /// `overrideMimeType()`: overrides the charset used to decode
    /// `responseText` and the type `responseXML` is parsed as.
    pub override_mime: Option<String>,
    pub status: u16,
    pub status_text: String,
    /// `responseURL`: the final post-redirect URL with its fragment stripped.
    pub response_url: String,
    pub response_headers: Vec<(String, String)>,
    pub response_body: Vec<u8>,
    /// Bytes received so far (`ProgressEvent.loaded`), and the `Content-Length`
    /// of the response when it had one (`total`, and `lengthComputable`). A
    /// cached or compressed response has no `Content-Length`, and then `total`
    /// stays 0 rather than being fabricated from what has arrived.
    pub loaded: f64,
    pub total: Option<f64>,
    /// The request body's length in bytes, recorded at `send()`.
    ///
    /// The upload half reports it verbatim: the body is handed to hyper whole,
    /// so once the response head proves it went out the honest figure is "all
    /// of it". Reporting 0 there — which is what an absent length degrades to —
    /// made every `xhr.upload.onprogress` bar read 0% (or `NaN`, dividing by a
    /// `total` of 0) at the moment the upload finished.
    pub upload_total: f64,
    pub request_id: Option<RequestId>,
    /// The spec's "response object" slot: `None` = not computed yet,
    /// `Some(None)` = computed and failed (unparseable JSON or document),
    /// `Some(Some(v))` = the object. Computing it once is observable — two
    /// reads of `xhr.response` must be the same object.
    pub response_object: Option<Option<JsValue>>,
    /// The document behind a cached document response. Its wrapper is the
    /// `response_object`, and holding that wrapper is what pins the node.
    pub response_document: Option<NodeId>,
    /// This object's slab key, which is also its
    /// [`crate::events::EventTargetKey::Host`] identity: listeners and the
    /// `onX` handler properties live in the shared registries under it, exactly
    /// as they do for a `new EventTarget()`.
    pub slab_key: u64,
    /// The wrapper object (for `event.target`); held strongly while the request
    /// has pending activity, which is what keeps a script-abandoned but
    /// in-flight XHR alive to deliver its events. It is released on every
    /// terminal transition so a finished request can be collected — and
    /// `open()` re-roots it, which is what makes a **reused** XHR fire events
    /// on its second request.
    pub wrapper: Option<JsValue>,
    /// `[SameObject]` `upload`, created on first access. Its own slab key is
    /// its own `EventTargetKey::Host`, so `xhr.upload.onprogress` is a
    /// different slot from `xhr.onprogress`.
    pub upload: Option<JsValue>,
    pub upload_key: u64,
}

/// An `XMLHttpRequest` receiver: its state plus the wrapper the call arrived
/// on. See [`crate::cx::BindCx::this_xhr`] for why the wrapper travels with it.
#[derive(Clone)]
pub(crate) struct XhrRef {
    pub data: Rc<RefCell<XhrData>>,
    pub wrapper: JsValue,
}

impl std::ops::Deref for XhrRef {
    type Target = Rc<RefCell<XhrData>>;
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl XhrData {
    /// Resets everything the response contributed to a **network error**:
    /// status 0, no status text, no headers, no body, no cached response
    /// object. Shared by `open()`, `abort()`, the error path and the timeout
    /// path — the spec's one "set response to network error" step.
    pub fn set_network_error(&mut self) {
        self.status = 0;
        self.status_text.clear();
        self.response_url.clear();
        self.response_headers.clear();
        self.response_body.clear();
        self.loaded = 0.0;
        self.total = None;
        self.response_object = None;
        self.response_document = None;
    }

    /// The response's `Content-Type`, or `None` when it had none.
    pub fn response_content_type(&self) -> Option<&str> {
        self.response_headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
    }
}

/// The accumulated response for an in-flight `fetch()`.
#[derive(Default)]
pub(crate) struct PendingResponse {
    pub status: u16,
    pub status_text: String,
    pub url: String,
    pub redirected: bool,
    pub response_type: ResponseType,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// An in-flight fetch/XHR awaiting net completion, keyed by [`RequestId`].
pub(crate) enum PendingNet {
    /// A `fetch()` promise: resolve with a `Response`, or reject on error.
    Fetch {
        resolve: JsValue,
        reject: JsValue,
        response: PendingResponse,
        /// The `AbortSignal` this fetch is registered with, if any. On
        /// completion its `pending_fetches` entry is pruned so the list stays
        /// bounded across a reused signal's many fetches.
        signal: Option<Rc<crate::state::AbortSignalData>>,
    },
    /// An `XMLHttpRequest`: update the XHR state and fire its events.
    Xhr { xhr: Rc<RefCell<XhrData>> },
}
