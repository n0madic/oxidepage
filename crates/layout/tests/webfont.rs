//! WP-A: WOFF/WOFF2 decode round-trips to sfnt with identical glyph data.
//!
//! `test.woff`/`test.woff2` are the WOFF flavours of `test.ttf` (all authored
//! by `assets/webfont/generate.py`). Decoding either flavour must reproduce a
//! font that skrifa loads with the same glyph count and byte-identical glyph
//! outlines as the raw `test.ttf` (WOFF2 re-encodes the `glyf` table, so we
//! compare decoded outlines, not raw bytes).

use oxidepage_layout::webfont::decode_font;
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::raw::TableProvider;
use skrifa::{FontRef, MetadataProvider};

const TTF: &[u8] = include_bytes!("../assets/webfont/test.ttf");
const WOFF: &[u8] = include_bytes!("../assets/webfont/test.woff");
const WOFF2: &[u8] = include_bytes!("../assets/webfont/test.woff2");

/// Records pen callbacks verbatim so two outlines can be compared for equality.
#[derive(Default, PartialEq, Debug)]
struct Recorder(Vec<String>);

impl OutlinePen for Recorder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.push(format!("M {x} {y}"));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.push(format!("L {x} {y}"));
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.0.push(format!("Q {cx} {cy} {x} {y}"));
    }
    fn curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.0.push(format!("C {c1x} {c1y} {c2x} {c2y} {x} {y}"));
    }
    fn close(&mut self) {
        self.0.push("Z".into());
    }
}

fn glyph_count(font: &FontRef<'_>) -> u16 {
    font.maxp().unwrap().num_glyphs()
}

fn outline_for(font: &FontRef<'_>, ch: char) -> Recorder {
    let gid = font.charmap().map(ch).expect("glyph for char");
    let glyphs = font.outline_glyphs();
    let glyph = glyphs.get(gid).expect("outline glyph");
    let mut pen = Recorder::default();
    glyph
        .draw(
            DrawSettings::unhinted(Size::unscaled(), LocationRef::default()),
            &mut pen,
        )
        .expect("draw");
    pen
}

fn assert_matches_ttf(decoded: &[u8]) {
    let reference = FontRef::new(TTF).expect("ttf loads");
    let font = FontRef::new(decoded).expect("decoded sfnt loads");
    assert_eq!(
        glyph_count(&font),
        glyph_count(&reference),
        "same glyph count"
    );
    for ch in ['A', 'F', 'O'] {
        assert_eq!(
            outline_for(&font, ch),
            outline_for(&reference, ch),
            "glyph outline for {ch:?} matches the TTF",
        );
    }
}

#[test]
fn ttf_passes_through_unchanged() {
    let decoded = decode_font(TTF).expect("ttf decodes");
    assert_eq!(decoded, TTF, "raw sfnt is returned verbatim");
}

#[test]
fn woff1_decodes_to_matching_sfnt() {
    let decoded = decode_font(WOFF).expect("woff decodes");
    assert_eq!(&decoded[0..4], &[0x00, 0x01, 0x00, 0x00], "sfnt signature");
    assert_matches_ttf(&decoded);
}

#[test]
fn woff2_decodes_to_matching_sfnt() {
    let decoded = decode_font(WOFF2).expect("woff2 decodes");
    assert_eq!(&decoded[0..4], &[0x00, 0x01, 0x00, 0x00], "sfnt signature");
    assert_matches_ttf(&decoded);
}

#[test]
fn unknown_signature_is_rejected() {
    assert!(decode_font(b"%PDF-1.7 not a font").is_none());
    assert!(decode_font(&[0x00, 0x00]).is_none(), "too short");
}
