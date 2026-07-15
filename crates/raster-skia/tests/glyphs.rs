//! WP-G: glyph rasterization tests using the bundled Ahem font, whose 'X'
//! is a solid em box straddling the baseline (ascent 0.8em, descent 0.2em).

use oxidepage_base::Size;
use oxidepage_paint::{
    Color, DisplayItem, DisplayList, FontId, FontResource, PositionedGlyph, ResourceTable,
    glyph_index,
};
use oxidepage_raster_skia::{RasterOptions, render};
use parley::fontique::Blob;

const AHEM: &[u8] = include_bytes!("../../layout/assets/Ahem.ttf");

/// A display list of one Ahem glyph run at 16px, baseline y = 16.
fn ahem_run(color: Color, glyph_xs: &[f32]) -> DisplayList {
    let blob = Blob::from(AHEM.to_vec());
    let font = FontId {
        blob: blob.id(),
        index: 0,
    };
    let gid = glyph_index(AHEM, 0, 'X').expect("Ahem maps X");
    let glyphs: Vec<PositionedGlyph> = glyph_xs
        .iter()
        .map(|&x| PositionedGlyph {
            id: gid,
            x,
            y: 16.0,
        })
        .collect();
    DisplayList {
        viewport: Size::new(64.0, 32.0),
        content_size: Size::new(64.0, 32.0),
        items: vec![DisplayItem::GlyphRun {
            font,
            size: 16.0,
            color,
            normalized_coords: Vec::new(),
            glyphs,
            debug_text: Some("X".into()),
        }],
        resources: ResourceTable {
            fonts: vec![FontResource {
                id: font,
                data: blob,
            }],
            ..ResourceTable::default()
        },
    }
}

fn is_blackish(p: [u8; 4]) -> bool {
    p[0] < 40 && p[1] < 40 && p[2] < 40 && p[3] > 200
}

#[test]
fn ahem_glyph_paints_a_solid_box() {
    let img = render(&ahem_run(Color::BLACK, &[0.0]), &RasterOptions::default());
    // Center of the em box (x≈8, y≈11) is inside the solid glyph.
    assert!(
        is_blackish(img.pixel(8, 11)),
        "center {:?}",
        img.pixel(8, 11)
    );
    // Well outside the glyph is the white base.
    assert_eq!(img.pixel(40, 25), [255, 255, 255, 255]);
}

#[test]
fn glyph_color_is_applied() {
    let img = render(
        &ahem_run(Color::rgb(255, 0, 0), &[0.0]),
        &RasterOptions::default(),
    );
    let p = img.pixel(8, 11);
    assert!(p[0] > 200 && p[1] < 40 && p[2] < 40, "center {p:?}");
}

#[test]
fn repeated_glyph_uses_cache_and_paints_both() {
    // Two 'X's advancing by one em; the second is a cache hit. Both boxes fill.
    let img = render(
        &ahem_run(Color::BLACK, &[0.0, 16.0]),
        &RasterOptions::default(),
    );
    assert!(
        is_blackish(img.pixel(8, 11)),
        "first {:?}",
        img.pixel(8, 11)
    );
    assert!(
        is_blackish(img.pixel(24, 11)),
        "second {:?}",
        img.pixel(24, 11)
    );
}

#[test]
fn dpr_scales_glyphs() {
    let img = render(
        &ahem_run(Color::BLACK, &[0.0]),
        &RasterOptions {
            scale: 2.0,
            ..RasterOptions::default()
        },
    );
    assert_eq!(img.width, 128);
    assert_eq!(img.height, 64);
    // The em box center at 2× DPR is around (16, 22).
    assert!(
        is_blackish(img.pixel(16, 22)),
        "center {:?}",
        img.pixel(16, 22)
    );
}
