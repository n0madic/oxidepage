//! Blocking `@import` loading (design doc §10, ADR-0005).
//!
//! Stylo's `@import` machinery is asynchronous (the loader returns a *pending*
//! rule that the embedder later fills in). We sidestep that with a synchronous,
//! blocking loader: `@import`ed sheets are fetched and parsed inline while the
//! parent sheet parses, guarded against cycles and capped at depth 8. Refused
//! or failed imports become `ImportSheet::Refused` so they never render.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use cssparser::SourceLocation;
use encoding_rs::Encoding;
use servo_arc::Arc as ServoArc;
use style::context::QuirksMode;
use style::media_queries::MediaList;
use style::shared_lock::{Locked, SharedRwLock};
use style::stylesheets::import_rule::{
    ImportLayer, ImportRule, ImportSheet, ImportSupportsCondition,
};
use style::stylesheets::{Origin, Stylesheet, StylesheetLoader, UrlExtraData};
use style::values::CssUrl;

/// The maximum `@import` nesting depth before further imports are refused.
const MAX_IMPORT_DEPTH: usize = 8;

/// Synchronously fetches CSS resources for the blocking `@import` loader.
pub trait CssFetcher {
    /// Fetches CSS bytes for `url`, returning the bytes, the charset from the
    /// `Content-Type` header (if any), and the final URL after redirects.
    ///
    /// # Errors
    /// Returns a human-readable message on network or protocol failure; the
    /// import is then refused (it does not render).
    fn fetch_css(&self, url: &url::Url) -> Result<(Vec<u8>, Option<String>, url::Url), String>;
}

/// A [`StylesheetLoader`] that resolves `@import` rules synchronously.
pub struct BlockingImportLoader<'a> {
    fetcher: &'a dyn CssFetcher,
    lock: SharedRwLock,
    origin: Origin,
    doc_encoding: Option<&'static Encoding>,
    depth: usize,
    /// Absolute URLs currently being loaded, for cycle detection.
    seen: Rc<RefCell<HashSet<String>>>,
}

impl<'a> BlockingImportLoader<'a> {
    /// Creates a loader for a top-level author stylesheet.
    #[must_use]
    pub fn new(
        fetcher: &'a dyn CssFetcher,
        lock: SharedRwLock,
        origin: Origin,
        doc_encoding: Option<&'static Encoding>,
    ) -> Self {
        Self {
            fetcher,
            lock,
            origin,
            doc_encoding,
            depth: 0,
            seen: Rc::new(RefCell::new(HashSet::new())),
        }
    }

    /// A child loader for a nested `@import`, one level deeper, sharing the
    /// cycle-detection set.
    fn child(&self) -> Self {
        Self {
            fetcher: self.fetcher,
            lock: self.lock.clone(),
            origin: self.origin,
            doc_encoding: self.doc_encoding,
            depth: self.depth + 1,
            seen: Rc::clone(&self.seen),
        }
    }

    fn load(
        &self,
        url: &CssUrl,
        media: &ServoArc<Locked<MediaList>>,
        supports: Option<&ImportSupportsCondition>,
    ) -> ImportSheet {
        // A false <supports-condition> never fetches.
        if supports.is_some_and(|s| !s.enabled) {
            return ImportSheet::new_refused();
        }
        if self.depth >= MAX_IMPORT_DEPTH {
            return ImportSheet::new_refused();
        }
        let Some(abs) = url.url() else {
            return ImportSheet::new_refused();
        };
        let abs_str = abs.as_str().to_owned();
        if self.seen.borrow().contains(&abs_str) {
            return ImportSheet::new_refused();
        }

        let (bytes, ct_charset, final_url) = match self.fetcher.fetch_css(abs) {
            Ok(result) => result,
            Err(_) => return ImportSheet::new_refused(),
        };
        self.seen.borrow_mut().insert(abs_str);

        let child = self.child();
        let sheet = Stylesheet::from_bytes(
            &bytes,
            UrlExtraData::from(final_url),
            ct_charset.as_deref(),
            self.doc_encoding,
            self.origin,
            media.clone(),
            self.lock.clone(),
            Some(&child),
            None,
            QuirksMode::NoQuirks,
        );
        ImportSheet::new(ServoArc::new(sheet))
    }
}

impl StylesheetLoader for BlockingImportLoader<'_> {
    fn request_stylesheet(
        &self,
        url: CssUrl,
        location: SourceLocation,
        _lock: &SharedRwLock,
        media: ServoArc<Locked<MediaList>>,
        supports: Option<ImportSupportsCondition>,
        layer: ImportLayer,
    ) -> ServoArc<Locked<ImportRule>> {
        let stylesheet = self.load(&url, &media, supports.as_ref());
        ServoArc::new(self.lock.wrap(ImportRule {
            url,
            stylesheet,
            supports,
            layer,
            source_location: location,
        }))
    }
}
