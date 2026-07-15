//! Glyph outline extraction via skrifa (ADR-0007 D1).
//!
//! Outlines are extracted once here as backend-neutral [`PathCommand`]s;
//! `oxidepage-raster-skia` converts them to `tiny_skia::Path` and
//! `oxidepage-export-pdf` to PDF path operators. Unhinted outlines are
//! deterministic across platforms (the reftest requirement). Outlines are
//! y-flipped to screen coordinates (y grows downward, origin on the baseline).

use skrifa::instance::{LocationRef, NormalizedCoord, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, GlyphId, MetadataProvider};

/// A backend-neutral path command in screen coordinates (y down), relative to
/// the glyph's pen origin on the baseline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathCommand {
    MoveTo {
        x: f32,
        y: f32,
    },
    LineTo {
        x: f32,
        y: f32,
    },
    QuadTo {
        cx: f32,
        cy: f32,
        x: f32,
        y: f32,
    },
    CurveTo {
        c1x: f32,
        c1y: f32,
        c2x: f32,
        c2y: f32,
        x: f32,
        y: f32,
    },
    Close,
}

/// Collects skrifa pen callbacks into [`PathCommand`]s, flipping the y axis
/// (font outlines are y-up, screen coordinates are y-down).
struct Collector {
    cmds: Vec<PathCommand>,
}

impl OutlinePen for Collector {
    fn move_to(&mut self, x: f32, y: f32) {
        self.cmds.push(PathCommand::MoveTo { x, y: -y });
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.cmds.push(PathCommand::LineTo { x, y: -y });
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.cmds.push(PathCommand::QuadTo {
            cx,
            cy: -cy,
            x,
            y: -y,
        });
    }
    fn curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.cmds.push(PathCommand::CurveTo {
            c1x,
            c1y: -c1y,
            c2x,
            c2y: -c2y,
            x,
            y: -y,
        });
    }
    fn close(&mut self) {
        self.cmds.push(PathCommand::Close);
    }
}

/// Extracts the outline of `glyph_id` from a font (`font_data` + collection
/// `index`) at `size_px`, applying the variable-font `coords`. Returns `None`
/// for an unparseable font, a missing/empty glyph (e.g. a space), or a draw
/// error.
#[must_use]
pub fn glyph_outline(
    font_data: &[u8],
    index: u32,
    glyph_id: u32,
    size_px: f32,
    coords: &[f32],
) -> Option<Vec<PathCommand>> {
    let font = FontRef::from_index(font_data, index).ok()?;
    let outlines = font.outline_glyphs();
    let glyph = outlines.get(GlyphId::new(glyph_id))?;

    let norm: Vec<NormalizedCoord> = coords
        .iter()
        .map(|&c| NormalizedCoord::from_f32(c))
        .collect();
    let settings = DrawSettings::unhinted(Size::new(size_px), LocationRef::new(&norm));

    let mut pen = Collector { cmds: Vec::new() };
    glyph.draw(settings, &mut pen).ok()?;
    if pen.cmds.is_empty() {
        return None;
    }
    Some(pen.cmds)
}

/// The glyph id for character `ch` in a font (`font_data` + collection
/// `index`), via the font's character map. Useful for tests and tools.
#[must_use]
pub fn glyph_index(font_data: &[u8], index: u32, ch: char) -> Option<u32> {
    let font = FontRef::from_index(font_data, index).ok()?;
    Some(font.charmap().map(ch)?.to_u32())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled Ahem font renders 'X' as a filled em square, so its outline
    /// spans roughly [0, size] horizontally and [-0.8·size, 0.2·size]
    /// vertically (baseline at 0, y down).
    #[test]
    fn ahem_x_outline_is_a_filled_box() {
        let bytes = include_bytes!("../../layout/assets/Ahem.ttf");
        let font = FontRef::new(bytes).expect("Ahem parses");
        // 'X' glyph id in Ahem: look it up through the charmap.
        let gid = font.charmap().map('X').expect("X mapped").to_u32();
        let cmds = glyph_outline(bytes, 0, gid, 100.0, &[]).expect("outline");

        let mut min_x = f32::MAX;
        let mut max_x = f32::MIN;
        let mut min_y = f32::MAX;
        let mut max_y = f32::MIN;
        for cmd in &cmds {
            if let PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } = *cmd {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
        // ~full em width, and the box straddles the baseline (top above,
        // bottom below).
        assert!(max_x - min_x > 90.0, "width {}", max_x - min_x);
        assert!(min_y < -50.0, "top above baseline: {min_y}");
        assert!(max_y > 10.0, "bottom below baseline: {max_y}");
    }
}
