//! Glyph rasterization: converts backend-neutral [`PathCommand`]s from
//! `oxidepage-paint` into `tiny_skia::Path`s, cached per (font, glyph, size)
//! on the renderer so repeated glyphs are built once (ADR-0007 D1, WP-G).

use std::collections::HashMap;
use std::rc::Rc;

use oxidepage_paint::{FontId, ResourceTable, emit_path_commands, glyph_outline};
use tiny_skia::Path;

use crate::path::PathBuilderSink;

/// Cache key for a rasterized glyph.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    blob: u64,
    index: u32,
    glyph: u32,
    /// Quantized size in quarter-pixels, so near-equal sizes share a path.
    size_q: u32,
    /// Hash of the (quantized) variable-font coordinates, so two variation
    /// instances of the same face never share a cached path. Static fonts
    /// (empty coords, e.g. Ahem) fold to a constant — a single cache entry.
    coords_key: u64,
}

/// Folds the variable-font normalized coordinates into one hashable value,
/// quantized to F2Dot14 units (parley's native normalized-coord grid).
fn coords_key(coords: &[f32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for &c in coords {
        let q = (c * 16384.0).round() as i32;
        h ^= u64::from(q as u32);
        h = h.wrapping_mul(0x100_0000_01b3); // FNV prime
    }
    h
}

/// A per-renderer cache of rasterized glyph paths (in glyph-local coordinates,
/// origin on the baseline).
#[derive(Default)]
pub(crate) struct GlyphCache {
    map: HashMap<GlyphKey, Option<Rc<Path>>>,
}

impl GlyphCache {
    /// The glyph's path in local coordinates, or `None` if it has no outline
    /// (a space) or the font is missing. The result is memoized. The returned
    /// `Rc` shares the cached path, so callers pay only a refcount bump (not a
    /// deep path clone) and are free of the cache borrow while drawing.
    pub(crate) fn glyph_path(
        &mut self,
        resources: &ResourceTable,
        font: FontId,
        glyph: u32,
        size: f32,
        coords: &[f32],
    ) -> Option<Rc<Path>> {
        let key = GlyphKey {
            blob: font.blob,
            index: font.index,
            glyph,
            size_q: (size * 4.0).round().max(0.0) as u32,
            coords_key: coords_key(coords),
        };
        self.map
            .entry(key)
            .or_insert_with(|| {
                resources
                    .font(font)
                    .and_then(|blob| {
                        let cmds = glyph_outline(blob.as_ref(), font.index, glyph, size, coords)?;
                        path_from_commands(&cmds)
                    })
                    .map(Rc::new)
            })
            .clone()
    }
}

/// Builds a `tiny_skia::Path` from backend-neutral path commands via the shared
/// [`emit_path_commands`] walker (identical geometry with the PDF backend).
fn path_from_commands(cmds: &[oxidepage_paint::PathCommand]) -> Option<Path> {
    let mut sink = PathBuilderSink(tiny_skia::PathBuilder::new());
    emit_path_commands(&mut sink, cmds);
    sink.0.finish()
}
