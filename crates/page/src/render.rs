//! Display-list construction, the "update the rendering" step, and raster /
//! screenshot output on the [`Page`] (Phase 6, ADR-0007 D6/D8).
//!
//! The display list is cached and rebuilt only when the [`PaintStamp`] changes
//! (DOM, style, viewport, or scroll). A rendering opportunity fires pending
//! `requestAnimationFrame` callbacks, flushes layout, and — when a consumer
//! (screenshot / PDF / display list) has asked for output — refreshes the
//! cached display list.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use oxidepage_layout::PaintStamp;
use oxidepage_paint::{Color, DisplayList, build_display_list, build_display_list_full};
use oxidepage_raster_skia::{
    RasterImage, RasterOptions, encode_png, render_full_page, render_scrolled,
};

use crate::Page;

/// Cached display list plus the paint stamp it was built for. Lives on the
/// [`Page`] (not the bindings' `PageState`) so it survives realm teardown.
#[derive(Default)]
pub(crate) struct RenderState {
    cache: RefCell<Option<Arc<DisplayList>>>,
    stamp: Cell<Option<PaintStamp>>,
    /// Set once a consumer asked for rendered output, so a rendering
    /// opportunity refreshes the cached display list.
    consumer: Cell<bool>,
}

impl RenderState {
    /// Drops the cached display list and stamp on navigation so a fresh
    /// document (whose new layout engine restarts its version counters at 0)
    /// cannot match a previous all-zero stamp and serve a stale display list.
    pub(crate) fn reset(&self) {
        *self.cache.borrow_mut() = None;
        self.stamp.set(None);
    }
}

impl Page {
    /// The current display list. Forces a rendering opportunity (firing any
    /// pending animation-frame callbacks and flushing layout) and rebuilds
    /// only when the paint stamp changed since the last build.
    #[must_use]
    pub fn display_list(&self) -> Arc<DisplayList> {
        self.render.consumer.set(true);
        self.update_the_rendering();
        self.build_cached_display_list()
    }

    /// The stable JSON dump of the current display list (for
    /// `--dump-display-list` and golden tests).
    #[must_use]
    pub fn display_list_json(&self) -> String {
        self.display_list().to_json()
    }

    /// Rasterizes the current display list to an RGBA image (for reftests).
    ///
    /// The list is built unscrolled and cached across scroll positions; the
    /// live document (viewport) scroll is applied here at raster time, so a
    /// scrolled viewport shifts document content while `position: fixed` content
    /// stays pinned.
    #[must_use]
    pub fn render_pixels(&self, options: &RasterOptions) -> RasterImage {
        let list = self.display_list();
        let scroll = self.state.layout.borrow().viewport_scroll();
        render_scrolled(&list, options, scroll)
    }

    /// Rasterizes the whole document — not just the viewport — to an RGBA image
    /// (see [`Page::screenshot_full_page`]).
    #[must_use]
    pub fn render_pixels_full_page(&self, options: &RasterOptions) -> RasterImage {
        let list = self.full_page_display_list();
        render_full_page(&list, options)
    }

    /// Renders a PNG screenshot of the viewport at device-pixel-ratio `dpr`
    /// (opaque white background).
    #[must_use]
    pub fn screenshot(&self, dpr: f32) -> Vec<u8> {
        let image = self.render_pixels(&RasterOptions {
            scale: dpr,
            background: Color::WHITE,
        });
        encode_png(&image).unwrap_or_default()
    }

    /// Renders a PNG screenshot of the whole document at device-pixel-ratio
    /// `dpr` (opaque white background).
    ///
    /// Like the PDF export, the image is sized to the document's `content_size`
    /// and painted from its top-left, ignoring the current viewport (document)
    /// scroll (ADR-0007 D8); element `overflow` scroll offsets still apply.
    /// Absurdly tall documents are clamped by the rasterizer's device-size caps.
    #[must_use]
    pub fn screenshot_full_page(&self, dpr: f32) -> Vec<u8> {
        let image = self.render_pixels_full_page(&RasterOptions {
            scale: dpr,
            background: Color::WHITE,
        });
        encode_png(&image).unwrap_or_default()
    }

    /// Exports the current page to a single-page PDF byte stream.
    ///
    /// The PDF is sized to the whole document (`content_size`), so — unlike a
    /// viewport screenshot — it is painted from the document's top-left and
    /// ignores the current viewport (document) scroll (ADR-0007 D8). Element
    /// `overflow` scroll offsets still apply.
    #[must_use]
    pub fn print_to_pdf(&self) -> Vec<u8> {
        let list = self.full_page_display_list();
        oxidepage_export_pdf::export(&list, &oxidepage_export_pdf::PdfOptions::default())
    }

    /// Builds a display list covering the whole document (unscrolled), after
    /// forcing a rendering opportunity. Not cached: the cache holds the
    /// viewport-scrolled list keyed by the paint stamp, which does not
    /// distinguish the two paint origins.
    fn full_page_display_list(&self) -> DisplayList {
        self.update_the_rendering();
        let dom = self.state.dom.borrow();
        let engine = self.state.layout.borrow();
        build_display_list_full(&dom, &engine)
    }

    /// The HTML "update the rendering" step (ADR-0007 D8): fire pending
    /// animation-frame callbacks with the elapsed-time timestamp, run a
    /// microtask checkpoint, flush layout, and refresh the cached display list
    /// when a consumer wants output.
    pub(crate) fn update_the_rendering(&self) {
        let callbacks = self.hooks.take_raf_callbacks();
        if !callbacks.is_empty() {
            let timestamp = self.start_time.get().elapsed().as_secs_f64() * 1000.0;
            self.with_cx(|cx| {
                for (id, callback) in &callbacks {
                    if self.hooks.take_raf_cancelled(*id) {
                        continue;
                    }
                    oxidepage_bindings::fire_raf_callback(cx, callback, timestamp);
                }
            });
        }
        self.flush_layout();
        if self.render.consumer.get() {
            self.build_cached_display_list();
        }
    }

    /// Returns the cached display list, rebuilding it if the paint stamp
    /// changed. Assumes layout is already flushed.
    fn build_cached_display_list(&self) -> Arc<DisplayList> {
        let dom = self.state.dom.borrow();
        let engine = self.state.layout.borrow();
        let stamp = engine.paint_stamp();

        if self.render.stamp.get() == Some(stamp)
            && let Some(cached) = self.render.cache.borrow().clone()
        {
            return cached;
        }

        let list = Arc::new(build_display_list(&dom, &engine));
        *self.render.cache.borrow_mut() = Some(Arc::clone(&list));
        self.render.stamp.set(Some(stamp));
        list
    }
}
