//! DisplayList → PDF export (design doc §5.9, Phase 6, ADR-0007 D3/M).
//!
//! Produces a single-page PDF sized `viewport.width × max(content_height,
//! viewport.height)` in CSS px, converted to points (×0.75). Fills and borders
//! become path operators. TrueType text is embedded as real, selectable text
//! (subset `Type0`/`CIDFontType2` fonts with `ToUnicode`, Phase 7, ADR-0008);
//! non-TrueType text falls back to vector outlines. Gradients become
//! axial/radial function shadings, images become embedded XObjects, and opacity
//! layers become ExtGState soft masks.

mod content;
mod fonts;

use std::io::Write;

use oxidepage_paint::DisplayList;
use pdf_writer::writers::Resources;
use pdf_writer::{Filter, Finish, Name, Pdf, Rect, Ref};

use crate::content::Built;

/// zlib-compresses `data` for a PDF `FlateDecode` stream. Writing into an
/// in-memory buffer never fails in practice.
fn deflate(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).expect("in-memory zlib write");
    encoder.finish().expect("zlib finish")
}

/// PDF export options.
#[derive(Clone, Copy, Debug)]
pub struct PdfOptions {
    /// CSS px → PDF point factor (96 dpi → 72 dpi = 0.75).
    pub px_to_pt: f32,
}

impl Default for PdfOptions {
    fn default() -> Self {
        Self { px_to_pt: 0.75 }
    }
}

/// Exports `list` to a single-page PDF byte stream.
#[must_use]
pub fn export(list: &DisplayList, options: &PdfOptions) -> Vec<u8> {
    let scale = options.px_to_pt;
    let page_w = list.viewport.width * scale;
    let page_h = list.content_size.height.max(list.viewport.height) * scale;

    // Reserve 1..=4 for the catalog, page tree, page, and content stream.
    let catalog_id = Ref::new(1);
    let pages_id = Ref::new(2);
    let page_id = Ref::new(3);
    let content_id = Ref::new(4);

    let built = content::build(list, scale, 5);

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(pages_id);
    pdf.pages(pages_id).kids([page_id]).count(1);

    {
        let mut page = pdf.page(page_id);
        page.parent(pages_id);
        page.media_box(Rect::new(0.0, 0.0, page_w, page_h));
        page.contents(content_id);
        let mut resources = page.resources();
        write_resources(&mut resources, &built);
        resources.finish();
        page.finish();
    }

    pdf.stream(content_id, &built.content).finish();

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
            write_resources(&mut resources, &built);
            resources.finish();
        }
        xobject.finish();
    }

    // Embedded, subsetted fonts (Type0 + CIDFontType2 + FontFile2 + ToUnicode).
    for font in &built.fonts {
        fonts::write_font(&mut pdf, &list.resources, font);
    }

    // Image XObjects (RGB) with a grayscale soft mask (alpha).
    for image in &built.images {
        let w = image.width as i32;
        let h = image.height as i32;
        let rgb: Vec<u8> = image
            .rgba
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        let alpha: Vec<u8> = image.rgba.chunks_exact(4).map(|p| p[3]).collect();
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

    pdf.finish()
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
