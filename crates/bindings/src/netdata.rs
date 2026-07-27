//! Rust-side data behind the Phase 3 network interfaces (URL, URLSearchParams,
//! Headers, Response, XMLHttpRequest) and the in-flight fetch/XHR bookkeeping.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxidepage_base::RequestId;
use oxidepage_js::{JsThrow, JsValue};
use oxidepage_net::ResponseType;
use url::Url;

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

/// A `FormData` entry list (the "entry list" of the XHR spec).
///
/// Values are strings only: `File`/`Blob` do not exist in this engine, so
/// `append(name, blob)` cannot happen — there is nothing to construct a Blob
/// with. Entry order is preserved, which is what both serializations rely on.
pub(crate) struct FormDataData {
    pub list: RefCell<Vec<(String, String)>>,
}

impl FormDataData {
    pub fn new(list: Vec<(String, String)>) -> Self {
        Self {
            list: RefCell::new(list),
        }
    }

    pub fn pairs(&self) -> Vec<(String, String)> {
        self.list.borrow().clone()
    }

    /// Serializes as `multipart/form-data` with the given boundary — the wire
    /// format a `FormData` body is sent in, and the reason a request carrying
    /// one must not have its `Content-Type` set by the caller (the header has to
    /// name the boundary, which only this code knows).
    pub fn to_multipart(&self, boundary: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, value) in self.pairs() {
            out.extend_from_slice(b"--");
            out.extend_from_slice(boundary.as_bytes());
            out.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
            out.extend_from_slice(escape_multipart_name(&name).as_bytes());
            out.extend_from_slice(b"\"\r\n\r\n");
            out.extend_from_slice(value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"--\r\n");
        out
    }
}

/// Per the Fetch spec's multipart serializer: a name may not carry a raw CR, LF
/// or `"` into the `Content-Disposition` header, so those three are escaped
/// rather than allowed to forge a header or terminate the field name early.
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

/// Handler-property callbacks on an XHR (event-handler IDL attributes).
/// An `XMLHttpRequest` object. All fields default to the UNSENT/empty state.
#[derive(Default)]
pub(crate) struct XhrData {
    pub ready_state: u16,
    pub method: String,
    pub url: String,
    /// `withCredentials`: send/store cookies on cross-origin requests.
    pub with_credentials: bool,
    pub request_headers: Vec<(String, String)>,
    pub response_type: String,
    pub status: u16,
    pub status_text: String,
    pub response_headers: Vec<(String, String)>,
    pub response_body: Vec<u8>,
    pub request_id: Option<RequestId>,
    /// This object's slab key, which is also its
    /// [`crate::events::EventTargetKey::Host`] identity: listeners and the
    /// `onX` handler properties live in the shared registries under it, exactly
    /// as they do for a `new EventTarget()`.
    pub slab_key: u64,
    /// The wrapper object (for `event.target`); held strongly while the request
    /// has pending activity. It is released on a terminal readyState (DONE /
    /// error / abort) so a completed request is not kept alive by this
    /// self-reference. Reusing an XHR after a terminal state (a rare pattern)
    /// therefore delivers later events with an undefined target.
    pub wrapper: Option<JsValue>,
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
