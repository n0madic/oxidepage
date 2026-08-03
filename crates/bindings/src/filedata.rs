//! Rust-side data behind the File API interfaces (`Blob`, `File`, `FileList`,
//! `FileReader`).
//!
//! One [`BlobData`] backs both byte-carrying interfaces (ADR-0032 D10). It
//! holds an `Rc<Vec<u8>>` plus a `[start, end)` window over it, so `slice()`
//! allocates nothing but the new view — which matters because the idiom it
//! exists for is chunked upload, where a naive copy would duplicate the whole
//! file once per chunk.
//!
//! `File`-ness is the presence of [`BlobData::file`], not a separate type: a
//! `File` *is* a `Blob` with a name and a timestamp, and modelling it as two
//! structs would mean two code paths for every byte-reading member.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxidepage_js::JsValue;

/// One file as the **embedder** supplies it (ADR-0032 D11).
///
/// Public because that is the only direction files travel: there is no
/// `DataTransfer` in this engine, so page script can never build a `File`
/// list — `DOM.setFileInputFiles` and the file chooser can, and they live
/// above this crate.
pub struct FileInput {
    pub name: String,
    /// `Rc`, so an embedder that hands the same buffer over repeatedly — which
    /// `input.files` does on every read — shares it rather than copying.
    pub bytes: Rc<Vec<u8>>,
    /// The MIME type; normalized on the way in, so a caller may pass the
    /// value it sniffed verbatim.
    pub content_type: String,
    /// `lastModified`, in Unix milliseconds.
    pub last_modified: i64,
}

/// The metadata that makes a [`BlobData`] a `File`.
pub(crate) struct FileMeta {
    pub name: String,
    /// `lastModified`, in Unix milliseconds.
    pub last_modified: i64,
}

/// The bytes and MIME type behind a `Blob` or a `File`.
pub(crate) struct BlobData {
    /// The whole backing buffer, shared by every view produced from it.
    pub bytes: Rc<Vec<u8>>,
    /// The normalized MIME type (lowercased, or empty when it was rejected).
    pub type_: String,
    /// This view's window into `bytes`; `start <= end <= bytes.len()` always.
    pub start: usize,
    pub end: usize,
    /// Present iff this is a `File`.
    pub file: Option<FileMeta>,
}

impl BlobData {
    /// A blob owning `bytes` whole.
    pub fn new(bytes: Vec<u8>, type_: String) -> Self {
        let end = bytes.len();
        Self {
            bytes: Rc::new(bytes),
            type_,
            start: 0,
            end,
            file: None,
        }
    }

    /// A `File` owning `bytes` whole.
    pub fn file(bytes: Vec<u8>, type_: String, name: String, last_modified: i64) -> Self {
        Self::shared_file(Rc::new(bytes), type_, name, last_modified)
    }

    /// A `File` over a buffer somebody else owns.
    ///
    /// The embedder's selected files live in `dom::SelectedFile` behind an `Rc`
    /// precisely so this can exist: `input.files` mints a fresh `FileList` on
    /// every read, so copying here would copy the whole upload each time a page
    /// touched the property.
    pub fn shared_file(
        bytes: Rc<Vec<u8>>,
        type_: String,
        name: String,
        last_modified: i64,
    ) -> Self {
        let end = bytes.len();
        Self {
            bytes,
            type_,
            start: 0,
            end,
            file: Some(FileMeta {
                name,
                last_modified,
            }),
        }
    }

    /// This view as a `File` with the given name, sharing the same bytes.
    ///
    /// A `FormData` entry's filename can differ from the blob it was built
    /// from, so reading the entry back has to produce a differently-named
    /// `File` over the same buffer rather than a copy.
    pub fn renamed(&self, name: &str) -> Self {
        Self {
            bytes: Rc::clone(&self.bytes),
            type_: self.type_.clone(),
            start: self.start,
            end: self.end,
            file: Some(FileMeta {
                name: name.to_owned(),
                last_modified: self.file.as_ref().map_or(0, |file| file.last_modified),
            }),
        }
    }

    /// This view's bytes.
    pub fn view(&self) -> &[u8] {
        &self.bytes[self.start..self.end]
    }

    pub fn size(&self) -> usize {
        self.end - self.start
    }

    /// The File API's `slice`: `start`/`end` are relative indices over *this*
    /// view (negative counts back from the end), clamped, and the result shares
    /// the same backing buffer.
    ///
    /// The result is always a plain `Blob`, never a `File` — slicing a file
    /// yields bytes, not a differently-named file, which is what the spec says
    /// and what keeps `f.slice(0, 1).name` from existing.
    pub fn slice(&self, start: Option<i64>, end: Option<i64>, type_: String) -> Self {
        let size = self.size() as i64;
        let resolve = |value: Option<i64>, default: i64| -> i64 {
            match value {
                None => default,
                Some(v) if v < 0 => (size + v).max(0),
                Some(v) => v.min(size),
            }
        };
        let rel_start = resolve(start, 0);
        let rel_end = resolve(end, size);
        let span = (rel_end - rel_start).max(0) as usize;
        let abs_start = self.start + rel_start as usize;
        Self {
            bytes: Rc::clone(&self.bytes),
            type_,
            start: abs_start,
            end: abs_start + span,
            file: None,
        }
    }
}

/// Normalizes a `type` member per the File API: it is lowercased, and it is
/// **rejected outright** (becoming the empty string) unless every code point is
/// in U+0020..U+007E.
///
/// Rejection rather than escaping is the spec's own answer, and it is the one
/// that matters here: the value ends up in a `Content-Type` header and in a
/// `data:` URL, so a value carrying a CR or a NUL would be header injection.
pub(crate) fn normalize_type(value: &str) -> String {
    if value.chars().any(|c| !('\u{20}'..='\u{7e}').contains(&c)) {
        return String::new();
    }
    value.to_ascii_lowercase()
}

/// `FileReader.readyState` values, mirroring the IDL constants.
pub(crate) const EMPTY: u16 = 0;
pub(crate) const LOADING: u16 = 1;
pub(crate) const DONE: u16 = 2;

/// A `FileReader`.
///
/// The wrapper is held for the object's life — the accepted wrapper-cycle leak
/// class [`crate::state::EventTargetData`] documents — because it is what
/// `event.target` hands back, and because a read in flight must survive script
/// dropping its last reference to the reader.
pub(crate) struct FileReaderData {
    pub ready_state: Cell<u16>,
    /// `result`: a string, an `ArrayBuffer`, or `null`.
    pub result: RefCell<JsValue>,
    /// `error`: a `DOMException`, or `null`.
    pub error: RefCell<JsValue>,
    /// This object's slab key, which is also its
    /// [`crate::events::EventTargetKey::Host`] identity.
    pub slab_key: Cell<u64>,
    pub wrapper: RefCell<Option<JsValue>>,
    /// Bumped by every read and by `abort()`. A queued completion task carries
    /// the token it was issued under and does nothing if it no longer matches,
    /// which is what makes `abort()` and a second `readAs*` cancel the first —
    /// the task is already on the embedder's queue and cannot be recalled.
    pub token: Cell<u64>,
}

impl Default for FileReaderData {
    fn default() -> Self {
        Self {
            ready_state: Cell::new(EMPTY),
            result: RefCell::new(JsValue::Null),
            error: RefCell::new(JsValue::Null),
            slab_key: Cell::new(0),
            wrapper: RefCell::new(None),
            token: Cell::new(0),
        }
    }
}

/// What a queued read produces once it runs.
#[derive(Clone, Copy)]
pub(crate) enum ReadKind {
    /// `readAsText(blob, encoding)`.
    Text,
    DataUrl,
    ArrayBuffer,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole design rests on (ADR-0032 D10): a slice is a
    /// *view*. Observable only from here — from script a copy and a view are
    /// indistinguishable, which is exactly why the invariant needs its own
    /// test rather than riding on a behavioral one.
    #[test]
    fn slice_shares_the_backing_buffer() {
        let blob = BlobData::new(b"0123456789".to_vec(), "text/plain".to_owned());
        let part = blob.slice(Some(2), Some(5), String::new());
        assert!(Rc::ptr_eq(&blob.bytes, &part.bytes));
        assert_eq!(part.view(), b"234");
        // And a slice of a slice stays on the same buffer, relative to its own
        // window rather than to the original's.
        let inner = part.slice(Some(1), None, String::new());
        assert!(Rc::ptr_eq(&blob.bytes, &inner.bytes));
        assert_eq!(inner.view(), b"34");
    }

    #[test]
    fn slice_clamps_relative_indices() {
        let blob = BlobData::new(b"0123456789".to_vec(), String::new());
        let s = |start, end| {
            String::from_utf8(blob.slice(start, end, String::new()).view().to_vec()).unwrap()
        };
        assert_eq!(s(None, None), "0123456789");
        assert_eq!(s(Some(-3), None), "789");
        assert_eq!(s(Some(-100), Some(2)), "01");
        assert_eq!(s(Some(100), Some(200)), "");
        // A reversed range is empty, not a panic on an underflowing span.
        assert_eq!(s(Some(5), Some(2)), "");
        // The saturated ends `arg_i64` can hand over must not overflow either.
        assert_eq!(s(Some(i64::MIN), Some(i64::MAX)), "0123456789");
    }

    /// Slicing a `File` yields a `Blob`: bytes, not a differently-named file.
    #[test]
    fn slice_drops_the_file_metadata() {
        let file = BlobData::file(b"abc".to_vec(), String::new(), "a.txt".to_owned(), 7);
        assert!(file.slice(None, None, String::new()).file.is_none());
    }

    #[test]
    fn a_type_is_lowercased_or_rejected_whole() {
        assert_eq!(normalize_type("TEXT/Plain"), "text/plain");
        assert_eq!(normalize_type(""), "");
        // The rejection is what keeps a `Content-Type` header and a `data:`
        // URL free of injected control characters.
        assert_eq!(normalize_type("text/plain\r\nX: y"), "");
        assert_eq!(normalize_type("text/plain\u{7f}"), "");
        assert_eq!(normalize_type("tëxt/plain"), "");
    }
}
