//! PDF font subsetting + embedding (Phase 7, WP-E, ADR-0008).
//!
//! Each TrueType font used by the content stream is subset to just its used
//! glyphs (via [`subsetter`]) and written as a `Type0` / `CIDFontType2` font
//! with an embedded `FontFile2`, a `CIDToGIDMap`, a per-glyph `W` width array
//! (from skrifa advances, scaled to the PDF 1000-unit em), and a `ToUnicode`
//! CMap built by reverse-mapping the font's charmap (gid → codepoint) so the
//! text is extractable/selectable.

use std::collections::{HashMap, HashSet};

use oxidepage_paint::ResourceTable;
use pdf_writer::types::{CidFontType, FontFlags, SystemInfo, UnicodeCmap};
use pdf_writer::{Finish, Name, Pdf, Rect, Str};
use skrifa::instance::{LocationRef, Size};
use skrifa::metrics::{GlyphMetrics, Metrics};
use skrifa::{FontRef, GlyphId, MetadataProvider};

use crate::content::FontUsage;

/// Writes all PDF objects for one embedded font: `Type0`, descendant
/// `CIDFontType2`, `FontDescriptor` (+ `FontFile2`), `CIDToGIDMap`, and
/// `ToUnicode`. The raw sfnt bytes are borrowed from the shared resource blob
/// (no per-font copy). `content::glyph_run` only registers fonts that parse as
/// single-face TrueType with a `glyf` table, so the `resources`/`FontRef`
/// guards here are defensive and never leave a dangling reference.
pub(crate) fn write_font(pdf: &mut Pdf, resources: &ResourceTable, usage: &FontUsage) {
    let Some(blob) = resources.font(usage.font) else {
        return;
    };
    let data = blob.as_ref();
    let index = usage.font.index;
    let Ok(font) = FontRef::from_index(data, index) else {
        return;
    };
    let metrics = Metrics::new(&font, Size::unscaled(), LocationRef::default());
    // PDF font metrics live in a 1000-unit em regardless of the font's design
    // units, so everything is scaled by 1000 / unitsPerEm.
    let upem = f32::from(metrics.units_per_em.max(1));
    let scale = 1000.0 / upem;
    let glyph_metrics = GlyphMetrics::new(&font, Size::unscaled(), LocationRef::default());

    // Reverse charmap for the used glyphs only (gid → first codepoint that maps
    // to it). Many codepoints may share a gid (ligatures/duplicates); the first
    // wins (v1 limitation, ADR-0008).
    let used_gids: HashSet<u16> = usage.remapper.remapped_gids().collect();
    let mut gid_to_codepoint: HashMap<u16, u32> = HashMap::new();
    for (codepoint, gid) in font.charmap().mappings() {
        if let Ok(gid) = u16::try_from(gid.to_u32())
            && used_gids.contains(&gid)
        {
            gid_to_codepoint.entry(gid).or_insert(codepoint);
        }
    }

    // Subset to the used glyphs. On failure, embed the full font — the
    // `CIDToGIDMap` maps CID → original gid in that case, so either path renders
    // and extracts correctly.
    let subset = subsetter::subset(data, index, &usage.remapper).ok();
    let subsetted = subset.is_some();
    let font_file_bytes = subset.unwrap_or_else(|| data.to_vec());

    // Per-CID data, iterated in CID (== subset gid) order.
    let mut widths: Vec<f32> = Vec::new();
    let mut cid_to_gid: Vec<u8> = Vec::new();
    let mut to_unicode = UnicodeCmap::new(
        Name(b"Adobe-Identity-UCS"),
        SystemInfo {
            registry: Str(b"Adobe"),
            ordering: Str(b"Identity"),
            supplement: 0,
        },
    );
    for (cid, original_gid) in usage.remapper.remapped_gids().enumerate() {
        let cid = cid as u16;
        let advance = glyph_metrics
            .advance_width(GlyphId::new(u32::from(original_gid)))
            .unwrap_or(0.0);
        widths.push(advance * scale);
        // When subsetted, the subset's gids equal the CIDs (identity); when the
        // full font is embedded, CID maps back to the original gid.
        let target_gid = if subsetted { cid } else { original_gid };
        cid_to_gid.extend_from_slice(&target_gid.to_be_bytes());
        if let Some(ch) = gid_to_codepoint
            .get(&original_gid)
            .and_then(|&cp| char::from_u32(cp))
        {
            to_unicode.pair(cid, ch);
        }
    }
    let to_unicode_bytes = to_unicode.finish().to_vec();

    let base_font_name = subset_font_name(&font_file_bytes);
    let base_font = Name(&base_font_name);

    {
        let mut type0 = pdf.type0_font(usage.type0);
        type0.base_font(base_font);
        type0.encoding_predefined(Name(b"Identity-H"));
        type0.descendant_font(usage.cid_font);
        type0.to_unicode(usage.to_unicode);
        type0.finish();
    }
    {
        let mut cid_font = pdf.cid_font(usage.cid_font);
        cid_font.subtype(CidFontType::Type2);
        cid_font.base_font(base_font);
        cid_font.system_info(SystemInfo {
            registry: Str(b"Adobe"),
            ordering: Str(b"Identity"),
            supplement: 0,
        });
        cid_font.font_descriptor(usage.descriptor);
        cid_font.cid_to_gid_map_stream(usage.cid_to_gid);
        {
            let mut w = cid_font.widths();
            w.consecutive(0, widths.iter().copied());
        }
        cid_font.finish();
    }
    {
        // Fallback bbox is expressed in font design units (like `bounds`), so it
        // scales correctly for any unitsPerEm; a `1000` literal here would be
        // wrongly re-scaled by `scale` for fonts whose upem != 1000.
        let (x0, y0, x1, y1) = metrics
            .bounds
            .map_or((0.0, metrics.descent, upem, metrics.ascent), |b| {
                (b.x_min, b.y_min, b.x_max, b.y_max)
            });
        // A font with no Basic-Latin coverage (icon/symbol / PUA-only font) is
        // flagged Symbolic; a text font is Nonsymbolic (matters for PDF/A).
        let symbolic = ['A', 'a', '0']
            .iter()
            .all(|&c| font.charmap().map(c).is_none());
        let flags = if symbolic {
            FontFlags::SYMBOLIC
        } else {
            FontFlags::NON_SYMBOLIC
        };
        let mut descriptor = pdf.font_descriptor(usage.descriptor);
        descriptor.name(base_font);
        descriptor.flags(flags);
        descriptor.bbox(Rect::new(x0 * scale, y0 * scale, x1 * scale, y1 * scale));
        descriptor.italic_angle(metrics.italic_angle);
        descriptor.ascent(metrics.ascent * scale);
        descriptor.descent(metrics.descent * scale);
        descriptor.cap_height(metrics.cap_height.unwrap_or(metrics.ascent) * scale);
        descriptor.stem_v(80.0);
        descriptor.font_file2(usage.font_file);
        descriptor.finish();
    }
    {
        // FontFile2: the (subset) sfnt, with the uncompressed length in Length1.
        let mut stream = pdf.stream(usage.font_file, &font_file_bytes);
        stream.pair(Name(b"Length1"), font_file_bytes.len() as i32);
        stream.finish();
    }
    pdf.stream(usage.cid_to_gid, &cid_to_gid).finish();
    pdf.stream(usage.to_unicode, &to_unicode_bytes).finish();
}

/// A deterministic 6-uppercase-letter subset tag + `+Embedded` base-font name
/// (the PDF convention for subset fonts), derived from the embedded bytes.
fn subset_font_name(data: &[u8]) -> Vec<u8> {
    use std::hash::Hash;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    // Fully qualified: `pdf_writer::Finish`'s blanket `finish()` is in scope and
    // would otherwise shadow `Hasher::finish`.
    let mut seed = std::hash::Hasher::finish(&hasher);

    let mut name = Vec::with_capacity(15);
    for _ in 0..6 {
        name.push(b'A' + (seed % 26) as u8);
        seed /= 26;
    }
    name.push(b'+');
    name.extend_from_slice(b"Embedded");
    name
}
