//! Display-list construction, the "update the rendering" step, and raster /
//! screenshot / PDF output on the [`Page`] (Phase 6, ADR-0007 D6/D8;
//! ADR-0026).
//!
//! The display list is cached and rebuilt only when the [`PaintStamp`] changes
//! (DOM, style, viewport, or scroll). A rendering opportunity fires pending
//! `requestAnimationFrame` callbacks, flushes layout, and — when a consumer
//! (screenshot / PDF / display list) has asked for output — refreshes the
//! cached display list.
//!
//! Capture options come in two layers, and that split is deliberate:
//! [`ScreenshotOptions`] and `PdfOptions` describe the *output* (area, format,
//! paper), while `PaintOptions` describes what the paint walk emits at all —
//! `print_background` has to be decided there, because by export time a
//! background is an ordinary fill.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use oxidepage_base::Rect;
use oxidepage_export_pdf::PdfOptions;
use oxidepage_layout::PaintStamp;
use oxidepage_paint::{Color, DisplayList, PaintOptions, build_display_list};
use oxidepage_raster_skia::{
    RasterImage, RasterOptions, encode_jpeg, encode_png, render_clipped, render_full_page,
    render_scrolled,
};

use crate::Page;

/// Encoding of a screenshot (`Page.captureScreenshot`'s `format`). WebP is a
/// documented non-goal (ADR-0026).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
}

/// Everything a screenshot can be asked for, in one struct — the shape CDP's
/// `Page.captureScreenshot` takes. [`Page::screenshot`] and
/// [`Page::screenshot_full_page`] stay as the two-argument-free wrappers.
#[derive(Clone, Copy, Debug)]
pub struct ScreenshotOptions {
    /// Device pixel ratio: the output is `ceil(area × dpr)` device px.
    pub dpr: f32,
    /// Capture the whole document rather than the viewport. Ignored when
    /// `clip` is set — a clip already names the area, in the same coordinates.
    pub full_page: bool,
    /// The area to capture, in **document** CSS px. `None` captures the
    /// viewport (or the document, with `full_page`).
    pub clip: Option<Rect>,
    pub format: ImageFormat,
    /// JPEG quality, 1..=100; ignored for PNG.
    pub quality: u8,
    /// The opaque base color painted under the page.
    pub background: Color,
}

impl Default for ScreenshotOptions {
    fn default() -> Self {
        Self {
            dpr: 1.0,
            full_page: false,
            clip: None,
            format: ImageFormat::Png,
            quality: 80,
            background: Color::WHITE,
        }
    }
}

/// Cached display list plus the paint stamp it was built for. Lives on the
/// [`Page`] (not the bindings' `WorldState`) so it survives realm teardown.
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
        let list = self.full_page_display_list(&PaintOptions::default());
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

    /// Renders a screenshot under the full [`ScreenshotOptions`] surface:
    /// viewport, whole document or an arbitrary document-space `clip`, as PNG
    /// or JPEG. An encoding failure yields an empty `Vec` — the same signal
    /// [`Page::screenshot`] gives.
    #[must_use]
    pub fn screenshot_with(&self, options: &ScreenshotOptions) -> Vec<u8> {
        let image = self.render_pixels_with(options);
        match options.format {
            ImageFormat::Png => encode_png(&image).unwrap_or_default(),
            ImageFormat::Jpeg => encode_jpeg(&image, options.quality).unwrap_or_default(),
        }
    }

    /// The RGBA pixels [`Page::screenshot_with`] encodes.
    ///
    /// A `clip` takes precedence over `full_page`: both name a capture area in
    /// document coordinates, and the explicit one wins.
    #[must_use]
    pub fn render_pixels_with(&self, options: &ScreenshotOptions) -> RasterImage {
        let raster = RasterOptions {
            scale: options.dpr,
            background: options.background,
        };
        match options.clip {
            // A clip names a region of the *document*, which can be anywhere —
            // so it takes the whole-document list, for the same reason
            // `render_pixels_full_page` does: the paint stamp cannot tell the
            // two paint origins apart, so the viewport cache must not serve a
            // capture that is not the viewport's.
            Some(clip) => render_clipped(
                &self.full_page_display_list(&PaintOptions::default()),
                &raster,
                clip,
            ),
            None if options.full_page => self.render_pixels_full_page(&raster),
            None => self.render_pixels(&raster),
        }
    }

    /// Exports the current page to a PDF byte stream with the default options:
    /// paginated A4, 0.4 in margins, backgrounds printed (ADR-0026).
    ///
    /// The PDF covers the whole document, so — unlike a viewport screenshot —
    /// it is painted from the document's top-left and ignores the current
    /// viewport (document) scroll (ADR-0007 D8). Element `overflow` scroll
    /// offsets still apply.
    #[must_use]
    pub fn print_to_pdf(&self) -> Vec<u8> {
        self.pdf(&PdfOptions::default(), &PaintOptions::default())
    }

    /// Exports the current page to PDF under explicit options.
    ///
    /// Two structs rather than one because they belong to different layers, and
    /// that is the point: `PdfOptions` is paper geometry, while
    /// `print_background` is a *paint*-time decision — by export time a
    /// background is an ordinary fill, indistinguishable from any other
    /// (ADR-0026).
    ///
    /// Where the pages break comes from layout, not from the display list:
    /// `DisplayItem`s carry baselines, never line tops, so the boundaries are
    /// `layout::pagination`'s class-A break points — the same "never cut a line
    /// in half" rule multi-column uses (ADR-0016).
    #[must_use]
    pub fn pdf(&self, options: &PdfOptions, paint: &PaintOptions) -> Vec<u8> {
        let list = self.full_page_display_list(paint);
        if !options.paginate {
            return oxidepage_export_pdf::export(&list, options);
        }
        // The same width the exporter's document box uses, so the slice height
        // layout fills against is the one the pages are actually drawn at.
        let page_height =
            options.page_content_height(list.content_size.width.max(list.viewport.width));
        let boundaries = self.state.layout.borrow().page_boundaries(page_height);
        oxidepage_export_pdf::export_paginated(&list, options, &boundaries)
    }

    /// Where the current document would break onto pages for a slice of
    /// `page_height` document CSS px (reflows first): `n + 1` offsets for `n`
    /// pages. [`Page::pdf`] feeds these to the exporter; an embedder laying out
    /// its own print preview wants the same numbers.
    #[must_use]
    pub fn page_boundaries(&self, page_height: f32) -> Vec<f32> {
        self.flush_layout();
        self.state.layout.borrow().page_boundaries(page_height)
    }

    /// Builds a display list covering the whole document (unscrolled), after
    /// forcing a rendering opportunity. Not cached: the cache holds the
    /// list keyed by the paint stamp, which knows nothing about
    /// [`PaintOptions`] and cannot tell a `print_background: false` build from
    /// the ordinary one.
    fn full_page_display_list(&self, options: &PaintOptions) -> DisplayList {
        self.update_the_rendering();
        let dom = self.state.dom.borrow();
        let engine = self.state.layout.borrow();
        build_display_list(&dom, &engine, options)
    }

    /// The HTML "update the rendering" step (ADR-0007 D8): fire pending
    /// animation-frame callbacks with the elapsed-time timestamp, run a
    /// microtask checkpoint, flush layout, and refresh the cached display list
    /// when a consumer wants output.
    pub(crate) fn update_the_rendering(&self) {
        let callbacks = self.hooks.take_raf_callbacks();
        if !callbacks.is_empty() {
            let timestamp = self.start_time.get().elapsed().as_secs_f64() * 1000.0;
            // Grouped by world so each is entered once, main world first.
            // A callback is a `JsValue` of the world that registered it and can
            // be invoked nowhere else (ADR-0033 D5).
            for world in self.worlds.all() {
                if !callbacks.iter().any(|(_, w, _)| *w == world.id) {
                    continue;
                }
                self.with_cx_in(world.id, |cx| {
                    for (id, w, callback) in &callbacks {
                        if *w != world.id || self.hooks.take_raf_cancelled(*id) {
                            continue;
                        }
                        oxidepage_bindings::fire_raf_callback(cx, callback, timestamp);
                    }
                });
            }
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

        let list = Arc::new(build_display_list(&dom, &engine, &PaintOptions::default()));
        *self.render.cache.borrow_mut() = Some(Arc::clone(&list));
        self.render.stamp.set(Some(stamp));
        list
    }
}
