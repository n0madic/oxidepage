//! DisplayList → PDF export (design doc §5.9, Phase 6, ADR-0007 D3/M,
//! ADR-0026).
//!
//! Fills and borders become path operators. TrueType text is embedded as real,
//! selectable text (subset `Type0`/`CIDFontType2` fonts with `ToUnicode`,
//! Phase 7, ADR-0008); non-TrueType text falls back to vector outlines.
//! Gradients become axial/radial function shadings, images become embedded
//! XObjects, and opacity layers become ExtGState soft masks.
//!
//! **Pagination.** The document's content is emitted **once**, as a Form
//! XObject in document coordinates; each page is a ~100-byte content stream that
//! clips to the page's content box, translates by its slice offset, and invokes
//! that one form. Slicing `list.items` instead would mean re-opening every
//! `PushClip`/`PushLayer` still open at the cut — the display list offers no way
//! to ask what is open at item *i* — and repeating the whole stream per page
//! would be O(pages × content). Where the slices fall is decided upstream, by
//! `layout::pagination`, because line-box tops are not in the display list.
//!
//! `PdfOptions { paginate: false }` restores the old single page, as tall as
//! the whole document.

mod content;
mod fonts;

use std::io::Write;

use oxidepage_paint::DisplayList;
use pdf_writer::writers::Resources;
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref};

use crate::content::Built;

/// zlib-compresses `data` for a PDF `FlateDecode` stream. Writing into an
/// in-memory buffer never fails in practice.
fn deflate(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).expect("in-memory zlib write");
    encoder.finish().expect("zlib finish")
}

/// Hard cap on the pages one export may produce, in the spirit of the engine's
/// other budgets: a pathological document must not turn into an unbounded file.
/// Matches `layout::pagination::MAX_PAGES`, which is where a `Page` export's
/// boundaries are capped first.
pub const MAX_PDF_PAGES: usize = 1000;

/// A paper size in **CSS px** (96 px per inch), portrait.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaperSize {
    pub width: f32,
    pub height: f32,
}

impl PaperSize {
    /// 210 × 297 mm.
    pub const A4: Self = Self {
        width: 793.7,
        height: 1122.52,
    };
    /// 297 × 420 mm.
    pub const A3: Self = Self {
        width: 1122.52,
        height: 1587.4,
    };
    /// 148 × 210 mm.
    pub const A5: Self = Self {
        width: 559.37,
        height: 793.7,
    };
    /// 8.5 × 11 in.
    pub const LETTER: Self = Self {
        width: 816.0,
        height: 1056.0,
    };
    /// 8.5 × 14 in.
    pub const LEGAL: Self = Self {
        width: 816.0,
        height: 1344.0,
    };
    /// 11 × 17 in.
    pub const TABLOID: Self = Self {
        width: 1056.0,
        height: 1632.0,
    };

    /// A named paper size, case-insensitively (`a4`, `letter`, `legal`, `a3`,
    /// `a5`, `tabloid`). `None` for anything else, so a caller can fall back to
    /// parsing an explicit `WxH`.
    #[must_use]
    pub fn by_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "a3" => Some(Self::A3),
            "a4" => Some(Self::A4),
            "a5" => Some(Self::A5),
            "letter" => Some(Self::LETTER),
            "legal" => Some(Self::LEGAL),
            "tabloid" => Some(Self::TABLOID),
            _ => None,
        }
    }
}

impl Default for PaperSize {
    fn default() -> Self {
        Self::A4
    }
}

/// Page margins in CSS px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Margins {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Margins {
    /// Chrome's default print margin: 0.4 in.
    pub const DEFAULT: f32 = 38.4;

    #[must_use]
    pub fn uniform(px: f32) -> Self {
        Self {
            top: px,
            right: px,
            bottom: px,
            left: px,
        }
    }
}

impl Default for Margins {
    fn default() -> Self {
        Self::uniform(Self::DEFAULT)
    }
}

/// PDF export options.
#[derive(Clone, Copy, Debug)]
pub struct PdfOptions {
    /// CSS px → PDF point factor (96 dpi → 72 dpi = 0.75).
    pub px_to_pt: f32,
    /// Paper size, portrait; see [`PdfOptions::landscape`].
    pub paper: PaperSize,
    /// Margins in CSS px.
    pub margins: Margins,
    /// Swap the paper's width and height.
    pub landscape: bool,
    /// User zoom, clamped to `0.1..=2.0` as Chrome does.
    pub scale: f32,
    /// Shrink wide content to the page's content width. There is no print-media
    /// relayout here (a documented non-goal), so a 1280 px document on A4 would
    /// otherwise simply run off the right edge. Never magnifies.
    pub fit_to_width: bool,
    /// Split the document across pages. `false` restores the pre-ADR-0026
    /// single page, as tall as the whole document.
    pub paginate: bool,
}

impl Default for PdfOptions {
    fn default() -> Self {
        Self {
            px_to_pt: 0.75,
            paper: PaperSize::default(),
            margins: Margins::default(),
            landscape: false,
            scale: 1.0,
            fit_to_width: true,
            paginate: true,
        }
    }
}

impl PdfOptions {
    /// The paper size in CSS px with [`Self::landscape`] applied.
    #[must_use]
    pub fn paper_size(&self) -> (f32, f32) {
        let (w, h) = (self.paper.width, self.paper.height);
        if self.landscape { (h, w) } else { (w, h) }
    }

    /// The page's content box in CSS px: the paper less its margins, floored at
    /// one pixel so absurd margins cannot produce a zero-sized (or inverted)
    /// content area.
    #[must_use]
    pub fn content_box(&self) -> (f32, f32) {
        let (w, h) = self.paper_size();
        (
            (w - self.margins.left - self.margins.right).max(1.0),
            (h - self.margins.top - self.margins.bottom).max(1.0),
        )
    }

    /// The factor document content is drawn at: fit-to-width (never above 1)
    /// times the user's [`Self::scale`].
    #[must_use]
    pub fn total_scale(&self, document_width: f32) -> f32 {
        let (content_w, _) = self.content_box();
        let fit = if self.fit_to_width && document_width > 0.0 {
            (content_w / document_width).min(1.0)
        } else {
            1.0
        };
        (fit * self.scale.clamp(0.1, 2.0)).max(f32::EPSILON)
    }

    /// How much of the document, in document CSS px, one page shows — the slice
    /// height `layout::pagination` fills against.
    #[must_use]
    pub fn page_content_height(&self, document_width: f32) -> f32 {
        let (_, content_h) = self.content_box();
        content_h / self.total_scale(document_width)
    }
}

/// The document box in CSS px — the area the display list paints into, and the
/// media box of an unpaginated export. Computed here alone: it used to be
/// derived in both `export` and `content::build`, which had to agree.
///
/// The **content** width, not the viewport's: fit-to-width measures against the
/// same number, and a document box narrower than it would shrink the page to fit
/// content that the form XObject's `/BBox` then clipped away (ADR-0026).
fn document_box(list: &DisplayList) -> (f32, f32) {
    (
        list.content_size.width.max(list.viewport.width),
        list.content_size.height.max(list.viewport.height),
    )
}

/// Exports `list` to a PDF byte stream, paginated per `options`.
///
/// The page breaks are uniform slices of the document — every
/// `options.page_content_height()` — because a bare `DisplayList` carries no
/// line boxes. [`export_paginated`] takes the real ones.
#[must_use]
pub fn export(list: &DisplayList, options: &PdfOptions) -> Vec<u8> {
    if !options.paginate {
        return export_single_page(list, options);
    }
    let (doc_w, doc_h) = document_box(list);
    let slice = options.page_content_height(doc_w);
    let mut boundaries = vec![0.0f32];
    while boundaries.len() <= MAX_PDF_PAGES {
        let next = boundaries.last().copied().unwrap_or(0.0) + slice;
        if next >= doc_h {
            break;
        }
        boundaries.push(next);
    }
    boundaries.push(doc_h);
    export_paginated(list, options, &boundaries)
}

/// Exports `list` across the pages named by `boundaries`: `n + 1` document-space
/// CSS-px offsets for `n` pages, the first at the document top and the last at
/// its bottom (`layout::pagination::page_boundaries` produces exactly that).
///
/// Out-of-order, non-finite and duplicate offsets are dropped, and the count is
/// capped at [`MAX_PDF_PAGES`]; a `paginate: false` option still wins, so the
/// two entry points cannot disagree about what the caller asked for.
#[must_use]
pub fn export_paginated(list: &DisplayList, options: &PdfOptions, boundaries: &[f32]) -> Vec<u8> {
    if !options.paginate {
        return export_single_page(list, options);
    }
    let (doc_w, doc_h) = document_box(list);
    let pages = normalize_boundaries(boundaries, doc_h);
    let page_count = pages.len() - 1;

    let scale = options.px_to_pt;
    let (paper_w, paper_h) = options.paper_size();
    let (content_w, content_h) = options.content_box();
    let total_scale = options.total_scale(doc_w);

    // 1: catalog, 2: page tree, 3: the document form XObject, then a
    // (page, content stream) pair each, then everything `content::build` needs.
    let catalog_id = Ref::new(1);
    let pages_id = Ref::new(2);
    let document_form_id = Ref::new(3);
    let page_ids: Vec<Ref> = (0..page_count)
        .map(|i| Ref::new(4 + 2 * i as i32))
        .collect();
    let content_ids: Vec<Ref> = (0..page_count)
        .map(|i| Ref::new(5 + 2 * i as i32))
        .collect();
    let built = content::build(list, scale, 4 + 2 * page_count as i32, doc_w, doc_h);

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(pages_id);
    pdf.pages(pages_id)
        .kids(page_ids.iter().copied())
        .count(page_count as i32);

    let document_form_name: &[u8] = b"Doc";
    for (index, page_id) in page_ids.iter().enumerate() {
        {
            let mut page = pdf.page(*page_id);
            page.parent(pages_id);
            page.media_box(Rect::new(0.0, 0.0, paper_w * scale, paper_h * scale));
            page.contents(content_ids[index]);
            let mut resources = page.resources();
            resources
                .x_objects()
                .pair(Name(document_form_name), document_form_id)
                .finish();
            resources.finish();
            page.finish();
        }
        let stream = page_content_stream(
            options,
            document_form_name,
            (pages[index], pages[index + 1]),
            doc_h,
            (paper_h, content_w, content_h),
            total_scale,
        );
        pdf.stream(content_ids[index], &stream).finish();
    }

    // The document's whole content, emitted once. Its form space is points with
    // the document's bottom-left at the origin, which is what the base CTM
    // inside `built.content` maps CSS px onto.
    {
        let mut xobject = pdf.form_xobject(document_form_id, &built.content);
        xobject.bbox(Rect::new(0.0, 0.0, doc_w * scale, doc_h * scale));
        {
            let mut resources = xobject.resources();
            write_resources(&mut resources, &built);
            resources.finish();
        }
        xobject.finish();
    }

    write_shared_objects(&mut pdf, list, &built);
    pdf.finish()
}

/// The whole document on one page, as tall as it is — the pre-ADR-0026 output,
/// kept for `PdfOptions { paginate: false }`.
fn export_single_page(list: &DisplayList, options: &PdfOptions) -> Vec<u8> {
    let scale = options.px_to_pt;
    let (doc_w, doc_h) = document_box(list);

    // Reserve 1..=4 for the catalog, page tree, page, and content stream.
    let catalog_id = Ref::new(1);
    let pages_id = Ref::new(2);
    let page_id = Ref::new(3);
    let content_id = Ref::new(4);

    let built = content::build(list, scale, 5, doc_w, doc_h);

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(pages_id);
    pdf.pages(pages_id).kids([page_id]).count(1);

    {
        let mut page = pdf.page(page_id);
        page.parent(pages_id);
        page.media_box(Rect::new(0.0, 0.0, doc_w * scale, doc_h * scale));
        page.contents(content_id);
        let mut resources = page.resources();
        write_resources(&mut resources, &built);
        resources.finish();
        page.finish();
    }

    pdf.stream(content_id, &built.content).finish();
    write_shared_objects(&mut pdf, list, &built);
    pdf.finish()
}

/// One page's content stream: clip to the page's **slice**, place it, invoke
/// the shared form.
///
/// The document form's space is points with the document's **bottom-left** at
/// the origin, so the slice starting at CSS `start` lands at
/// `top_edge − scale × (doc_h − start)` on a y-up page.
///
/// The clip height is the slice `end − start`, never the page's full content
/// height: the fill normally stops at a break opportunity *above* the page
/// bottom, and the strip below it holds the **next** page's content, which
/// would otherwise show through at the foot of this one and appear twice. Same
/// rule, same reason as the per-column clip in `paint::builder`.
fn page_content_stream(
    options: &PdfOptions,
    form: &[u8],
    (start, end): (f32, f32),
    doc_h: f32,
    (paper_h, content_w, content_h): (f32, f32, f32),
    total_scale: f32,
) -> Vec<u8> {
    let pt = options.px_to_pt;
    let left = options.margins.left * pt;
    let top_edge = (paper_h - options.margins.top) * pt;
    // A slice can only be shorter than the page (the fill stops early) or, for
    // unbreakable content, longer — in which case it is the page that bounds it.
    let slice = ((end - start) * total_scale * pt).clamp(0.0, content_h * pt);
    let bottom = top_edge - slice;

    let mut content = Content::new();
    content.rect(left, bottom, content_w * pt, slice);
    content.clip_nonzero();
    content.end_path();
    content.transform([
        total_scale,
        0.0,
        0.0,
        total_scale,
        left,
        top_edge - total_scale * (doc_h - start) * pt,
    ]);
    content.x_object(Name(form));
    content.finish().to_vec()
}

/// Sanitizes caller-supplied page boundaries into `n + 1` strictly increasing
/// offsets spanning `[0, doc_h]`, with at least one page and at most
/// [`MAX_PDF_PAGES`].
fn normalize_boundaries(boundaries: &[f32], doc_h: f32) -> Vec<f32> {
    const EPS: f32 = 0.01;
    let mut out = vec![0.0f32];
    for &offset in boundaries {
        if !offset.is_finite() || offset <= out[out.len() - 1] + EPS || offset >= doc_h - EPS {
            continue;
        }
        out.push(offset);
        if out.len() >= MAX_PDF_PAGES {
            break;
        }
    }
    out.push(doc_h.max(EPS));
    out
}

/// Everything after the pages: layer forms, fonts, images, shadings, gstates.
/// Shared by both emission paths so they cannot drift.
fn write_shared_objects(pdf: &mut Pdf, list: &DisplayList, built: &Built) {
    // Layer transparency-group form XObjects. Each shares the page's full
    // resource set (globally named/allocated), so it can reference any image,
    // shading, gstate, font, or nested form by the same name.
    for form in &built.forms {
        let mut xobject = pdf.form_xobject(form.obj, &form.content);
        xobject.bbox(Rect::new(
            form.bbox[0],
            form.bbox[1],
            form.bbox[2],
            form.bbox[3],
        ));
        {
            let mut group = xobject.group();
            group.transparency();
            group.isolated(true);
            group.color_space().device_rgb();
        }
        {
            let mut resources = xobject.resources();
            write_resources(&mut resources, built);
            resources.finish();
        }
        xobject.finish();
    }

    // Embedded, subsetted fonts (Type0 + CIDFontType2 + FontFile2 + ToUnicode).
    for font in &built.fonts {
        fonts::write_font(pdf, &list.resources, font);
    }

    // Image XObjects (RGB) with a grayscale soft mask (alpha).
    for image in &built.images {
        let w = image.width as i32;
        let h = image.height as i32;
        let rgb: Vec<u8> = image
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        let alpha: Vec<u8> = image.rgba.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        // Embed the raw samples FlateDecode-compressed rather than uncompressed
        // (finding L4): image data dominates PDF size otherwise.
        let rgb = deflate(&rgb);
        let alpha = deflate(&alpha);

        {
            let mut mask = pdf.image_xobject(image.smask, &alpha);
            mask.filter(Filter::FlateDecode);
            mask.width(w);
            mask.height(h);
            mask.color_space().device_gray();
            mask.bits_per_component(8);
            mask.finish();
        }
        {
            let mut xobject = pdf.image_xobject(image.obj, &rgb);
            xobject.filter(Filter::FlateDecode);
            xobject.width(w);
            xobject.height(h);
            xobject.color_space().device_rgb();
            xobject.bits_per_component(8);
            xobject.s_mask(image.smask);
            xobject.finish();
        }
    }

    // Gradient shadings + their color-ramp functions. Every stop and offset is
    // honored via a stitching (Type 3) function over the sorted stops, with a
    // linear exponential (Type 2) segment per adjacent pair; a lone segment
    // needs no stitching parent (finding M1).
    for shading in &built.shadings {
        if shading.segments.is_empty() {
            // A single segment: the exponential is the color function directly.
            let c0 = shading.stops.first().map_or([0.0; 3], |s| s.1);
            let c1 = shading.stops.last().map_or(c0, |s| s.1);
            let mut function = pdf.exponential_function(shading.function);
            function.domain([0.0, 1.0]);
            function.c0(c0);
            function.c1(c1);
            function.n(1.0);
            function.finish();
        } else {
            // One exponential per stop pair, stitched over the stop offsets.
            for (i, seg) in shading.segments.iter().enumerate() {
                let mut function = pdf.exponential_function(*seg);
                function.domain([0.0, 1.0]);
                function.c0(shading.stops[i].1);
                function.c1(shading.stops[i + 1].1);
                function.n(1.0);
                function.finish();
            }
            let mut stitch = pdf.stitching_function(shading.function);
            stitch.domain([0.0, 1.0]);
            stitch.functions(shading.segments.iter().copied());
            // Interior stop offsets (one fewer than the segment count).
            let interior = shading.stops.len() - 1;
            stitch.bounds(shading.stops[1..interior].iter().map(|s| s.0));
            // Each segment maps its subdomain onto the exponential's [0, 1].
            stitch.encode(shading.segments.iter().flat_map(|_| [0.0, 1.0]));
            stitch.finish();
        }
        {
            let mut sh = pdf.function_shading(shading.obj);
            sh.shading_type(shading.kind);
            sh.color_space().device_rgb();
            sh.coords(shading.coords.iter().copied());
            sh.function(shading.function);
            sh.extend(shading.extend);
            sh.finish();
        }
    }

    // Opacity ExtGStates. Both `ca` and `CA` are set so the alpha applies to
    // the layer's form-XObject invocation (a non-stroking paint) and to any
    // stroking within it.
    for gstate in &built.gstates {
        let mut gs = pdf.ext_graphics(gstate.obj);
        gs.non_stroking_alpha(gstate.alpha);
        gs.stroking_alpha(gstate.alpha);
        gs.finish();
    }
}

/// Writes a `/Resources` dictionary listing every resource the content (page or
/// a layer form XObject) may reference. All resources are globally named and
/// allocated, so listing the full set is valid for any content stream.
fn write_resources(resources: &mut Resources<'_>, built: &Built) {
    if !built.images.is_empty() || !built.forms.is_empty() {
        let mut dict = resources.x_objects();
        for image in &built.images {
            dict.pair(Name(&image.name), image.obj);
        }
        for form in &built.forms {
            dict.pair(Name(&form.name), form.obj);
        }
        dict.finish();
    }
    if !built.shadings.is_empty() {
        let mut dict = resources.shadings();
        for shading in &built.shadings {
            dict.pair(Name(&shading.name), shading.obj);
        }
        dict.finish();
    }
    if !built.gstates.is_empty() {
        let mut dict = resources.ext_g_states();
        for gstate in &built.gstates {
            dict.pair(Name(&gstate.name), gstate.obj);
        }
        dict.finish();
    }
    if !built.fonts.is_empty() {
        let mut dict = resources.fonts();
        for font in &built.fonts {
            dict.pair(Name(&font.name), font.type0);
        }
        dict.finish();
    }
}
