//! Structural PDF export tests — header + media box math, embedded/subset
//! `Type0` text with a `ToUnicode` CMap (Phase 7), the outline fallback for
//! unembeddable fonts, image XObjects, and gradient shadings. The output is
//! uncompressed, so markers are checked as text.

use std::sync::Arc;

use oxidepage_base::{Point, Rect, Size, Transform2D};
use oxidepage_export_pdf::{PdfOptions, export};
use oxidepage_paint::{
    BorderRadii, Brush, Color, DecodedImage, DisplayItem, DisplayList, ExtendMode, FontId,
    FontResource, GradientStop, ImageData, ImageId, LinearGradient, PositionedGlyph,
    RadialGradient, ResourceTable, glyph_index,
};
use parley::fontique::Blob;

const AHEM: &[u8] = include_bytes!("../../layout/assets/Ahem.ttf");

fn list(items: Vec<DisplayItem>, resources: ResourceTable, content_h: f32) -> DisplayList {
    DisplayList {
        viewport: Size::new(800.0, 600.0),
        content_size: Size::new(800.0, content_h),
        items,
        resources,
    }
}

fn text(pdf: &[u8]) -> String {
    String::from_utf8_lossy(pdf).into_owned()
}

#[test]
fn header_and_media_box_math() {
    // ADR-0026 made pagination the default, so the media box is the *paper*.
    // The document-sized page it used to be is `paginate: false`, asserted just
    // below.
    let pdf = export(
        &list(
            vec![DisplayItem::Fill {
                rect: Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
                radii: BorderRadii::ZERO,
                brush: Brush::Solid(Color::rgb(255, 0, 0)),
            }],
            ResourceTable::default(),
            600.0,
        ),
        &PdfOptions {
            paginate: false,
            ..PdfOptions::default()
        },
    );
    assert_eq!(&pdf[0..5], b"%PDF-", "PDF header");
    let s = text(&pdf);
    // 800×600 CSS px × 0.75 = 600×450 pt.
    assert!(
        s.contains("/MediaBox [0 0 600 450]"),
        "media box 600×450: {s}"
    );
    assert!(
        s.contains("/Catalog") && s.contains("/Page"),
        "structure present"
    );
    assert!(s.contains("%%EOF"), "trailer");
}

#[test]
fn tall_content_extends_page_height() {
    // Unpaginated, the single page is as tall as the document:
    // max(1200, 600) × 0.75 = 900 pt.
    let pdf = export(
        &list(vec![], ResourceTable::default(), 1200.0),
        &PdfOptions {
            paginate: false,
            ..PdfOptions::default()
        },
    );
    assert!(
        text(&pdf).contains("/MediaBox [0 0 600 900]"),
        "page height 900"
    );
}

/// Builds a PDF from a single-glyph run of the given `char`, returning the raw
/// (uncompressed) PDF bytes.
fn pdf_with_glyph(ch: char) -> Vec<u8> {
    let blob = Blob::from(AHEM.to_vec());
    let font = FontId {
        blob: blob.id(),
        index: 0,
    };
    let gid = glyph_index(AHEM, 0, ch).unwrap();
    let mut resources = ResourceTable::default();
    resources.add_font(FontResource {
        id: font,
        data: blob,
    });
    export(
        &list(
            vec![DisplayItem::GlyphRun {
                font,
                size: 20.0,
                color: Color::BLACK,
                normalized_coords: Vec::new(),
                glyphs: vec![PositionedGlyph {
                    id: gid,
                    x: 10.0,
                    y: 20.0,
                }],
                debug_text: Some(ch.to_string()),
            }],
            resources,
            600.0,
        ),
        &PdfOptions::default(),
    )
}

#[test]
fn truetype_text_embeds_a_subset_type0_font() {
    let pdf = pdf_with_glyph('X');
    let s = text(&pdf);
    // Phase 7 contract: TrueType text is real, selectable, subset text.
    assert!(s.contains("/Font"), "font dictionary: {s}");
    assert!(s.contains("/Type0"), "composite font");
    assert!(s.contains("/CIDFontType2"), "TrueType CID font");
    assert!(s.contains("/FontFile2"), "embedded (subset) font program");
    assert!(s.contains("/ToUnicode"), "ToUnicode CMap for extraction");
    assert!(s.contains("BT") && s.contains("ET"), "text object");
    assert!(s.contains("Tj"), "glyph show operator");
}

/// Extracts the `beginbfchar … endbfchar` body of the ToUnicode CMap.
fn bfchar_block(pdf: &str) -> String {
    let start = pdf.find("beginbfchar").expect("beginbfchar present");
    let end = pdf[start..].find("endbfchar").expect("endbfchar present") + start;
    pdf[start..end].to_owned()
}

#[test]
fn to_unicode_maps_the_glyphs_codepoint() {
    // 'X' is U+0058; its ToUnicode bfchar entry maps the glyph's CID to the
    // UTF-16 hex `0058`, so extracted text recovers the original character.
    let pdf = pdf_with_glyph('X');
    let s = text(&pdf);
    // Assert the codepoint inside the bfchar block (not anywhere in the file, so
    // an xref offset or FontFile2 byte cannot spuriously satisfy it).
    let block = bfchar_block(&s);
    assert!(
        block.contains("<0058>"),
        "'X' (U+0058) is mapped in the ToUnicode bfchar block: {block}"
    );
}

#[test]
fn corrupt_truetype_falls_back_to_outlines_without_a_dangling_ref() {
    // A blob with a valid TrueType signature but a malformed body must NOT take
    // the embed path (no /Font object left dangling); it renders as outlines.
    let mut corrupt = vec![0x00, 0x01, 0x00, 0x00];
    corrupt.extend_from_slice(&[0xFF; 64]);
    let blob = Blob::from(corrupt);
    let font = FontId {
        blob: blob.id(),
        index: 0,
    };
    let mut resources = ResourceTable::default();
    resources.add_font(FontResource {
        id: font,
        data: blob,
    });
    let pdf = export(
        &list(
            vec![DisplayItem::GlyphRun {
                font,
                size: 20.0,
                color: Color::BLACK,
                normalized_coords: Vec::new(),
                glyphs: vec![PositionedGlyph {
                    id: 3,
                    x: 10.0,
                    y: 20.0,
                }],
                debug_text: Some("?".into()),
            }],
            resources,
            600.0,
        ),
        &PdfOptions::default(),
    );
    let s = text(&pdf);
    // No font dictionary was emitted (fell back), and the PDF is still valid.
    assert!(!s.contains("/Type0"), "unparseable font not embedded: {s}");
    assert!(!s.contains("/FontFile2"), "no embedded program");
    assert_eq!(&pdf[0..5], b"%PDF-", "still a valid PDF header");
    assert!(
        s.contains("%%EOF"),
        "valid trailer (no dangling references)"
    );
}

#[test]
fn image_embeds_an_xobject() {
    let image = Arc::new(DecodedImage {
        id: ImageId(1),
        width: 2,
        height: 2,
        data: ImageData::Raster {
            rgba: Arc::new(vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128,
            ]),
        },
    });
    let mut resources = ResourceTable::default();
    resources.add_image(image);
    let pdf = export(
        &list(
            vec![DisplayItem::Image {
                dst: Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
                image: ImageId(1),
                tile: oxidepage_paint::TileMode::Stretch,
                radii: BorderRadii::ZERO,
            }],
            resources,
            600.0,
        ),
        &PdfOptions::default(),
    );
    let s = text(&pdf);
    assert!(s.contains("/XObject"), "xobject resource");
    assert!(s.contains("/Image"), "image subtype");
    assert!(s.contains("/SMask"), "soft mask for alpha");
}

/// An 8×8 SVG — a whole-canvas red square.
fn vector_image(id: u64) -> Arc<DecodedImage> {
    Arc::new(DecodedImage {
        id: ImageId(id),
        width: 8,
        height: 8,
        data: ImageData::Vector {
            svg: Arc::new(
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="red"/></svg>"#
                    .to_vec(),
            ),
        },
    })
}

/// PDF has no vector-image XObject to hand SVG to, so a vector image is
/// rasterized for embedding — but at `PDF_SVG_SCALE` × the destination rect, not
/// at its intrinsic size. An 8×8 icon printed into a 40×40 box embeds at 120×120,
/// so it stays sharp on paper instead of being a 5× upscale of 8×8 pixels.
#[test]
fn vector_image_embeds_at_the_scaled_destination_size() {
    let mut resources = ResourceTable::default();
    resources.add_image(vector_image(1));
    let pdf = export(
        &list(
            vec![DisplayItem::Image {
                dst: Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
                image: ImageId(1),
                tile: oxidepage_paint::TileMode::Stretch,
                radii: BorderRadii::ZERO,
            }],
            resources,
            600.0,
        ),
        &PdfOptions::default(),
    );
    let s = text(&pdf);
    assert!(s.contains("/XObject"), "xobject resource");
    assert!(
        s.contains("/Width 120") && s.contains("/Height 120"),
        "40px destination at 3x embeds a 120x120 raster, not the 8x8 intrinsic one: {s}"
    );
}

/// The same SVG drawn at two sizes is two XObjects: deduplicating on the image id
/// alone would reuse the small rasterization for the large box — exactly the
/// blurry upscale this path exists to avoid.
#[test]
fn one_vector_image_at_two_sizes_embeds_two_xobjects() {
    let mut resources = ResourceTable::default();
    resources.add_image(vector_image(1));
    let item = |x: f32, side: f32| DisplayItem::Image {
        dst: Rect::from_xywh(x, 0.0, side, side),
        image: ImageId(1),
        tile: oxidepage_paint::TileMode::Stretch,
        radii: BorderRadii::ZERO,
    };
    let pdf = export(
        &list(vec![item(0.0, 40.0), item(200.0, 100.0)], resources, 600.0),
        &PdfOptions::default(),
    );
    let s = text(&pdf);
    assert!(
        s.contains("/Width 120"),
        "the 40px box embeds at 120px: {s}"
    );
    assert!(
        s.contains("/Width 300"),
        "the 100px box embeds at 300px: {s}"
    );
}

#[test]
fn gradient_embeds_a_shading() {
    let pdf = export(
        &list(
            vec![DisplayItem::Fill {
                rect: Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
                radii: BorderRadii::ZERO,
                brush: Brush::LinearGradient(LinearGradient {
                    start: Point::new(0.0, 0.0),
                    end: Point::new(100.0, 0.0),
                    stops: vec![
                        GradientStop {
                            offset: 0.0,
                            color: Color::rgb(255, 0, 0),
                        },
                        GradientStop {
                            offset: 1.0,
                            color: Color::rgb(0, 0, 255),
                        },
                    ],
                    extend: oxidepage_paint::ExtendMode::Pad,
                }),
            }],
            ResourceTable::default(),
            600.0,
        ),
        &PdfOptions::default(),
    );
    let s = text(&pdf);
    assert!(s.contains("/Shading"), "shading resource: {s}");
    assert!(s.contains("/ShadingType"), "shading type");
    assert!(s.contains("/Function"), "shading function");
}

/// Builds a PDF from a single linear-gradient fill with the given stops.
fn pdf_with_linear_stops(stops: Vec<GradientStop>) -> Vec<u8> {
    export(
        &list(
            vec![DisplayItem::Fill {
                rect: Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
                radii: BorderRadii::ZERO,
                brush: Brush::LinearGradient(LinearGradient {
                    start: Point::new(0.0, 0.0),
                    end: Point::new(100.0, 0.0),
                    stops,
                    extend: ExtendMode::Pad,
                }),
            }],
            ResourceTable::default(),
            600.0,
        ),
        &PdfOptions::default(),
    )
}

#[test]
fn three_stop_gradient_emits_a_stitching_function() {
    // red, green, blue at 0/0.5/1 → two segments → a Type 3 stitching function
    // over two Type 2 exponentials (the intermediate green stop is honored, not
    // collapsed to a red→blue ramp, finding M1).
    let pdf = pdf_with_linear_stops(vec![
        GradientStop {
            offset: 0.0,
            color: Color::rgb(255, 0, 0),
        },
        GradientStop {
            offset: 0.5,
            color: Color::rgb(0, 255, 0),
        },
        GradientStop {
            offset: 1.0,
            color: Color::rgb(0, 0, 255),
        },
    ]);
    let s = text(&pdf);
    assert!(s.contains("/FunctionType 3"), "stitching function: {s}");
    let exponentials = s.matches("/FunctionType 2").count();
    assert_eq!(exponentials, 2, "one exponential per stop pair");
}

#[test]
fn gradient_stop_offsets_are_honored_via_bounds() {
    // red 30%, blue 70%: offsets must be preserved. The ramp is augmented with
    // constant leading/trailing segments so the stitching function has bounds
    // at 0.3 and 0.7 (three segments), not a plain red→blue exponential over
    // the whole domain (finding M1).
    let pdf = pdf_with_linear_stops(vec![
        GradientStop {
            offset: 0.3,
            color: Color::rgb(255, 0, 0),
        },
        GradientStop {
            offset: 0.7,
            color: Color::rgb(0, 0, 255),
        },
    ]);
    let s = text(&pdf);
    assert!(s.contains("/FunctionType 3"), "stitching function: {s}");
    assert_eq!(
        s.matches("/FunctionType 2").count(),
        3,
        "constant + ramp + constant segments"
    );
    assert!(s.contains("/Bounds"), "stitching bounds present");
    // The interior stop offsets appear as the stitching bounds.
    let bounds_start = s.find("/Bounds").expect("bounds");
    let bounds = &s[bounds_start..bounds_start + 40];
    assert!(bounds.contains("0.3") && bounds.contains("0.7"), "{bounds}");
}

#[test]
fn elliptical_radial_gradient_emits_a_y_scale_transform() {
    // A radial gradient over a non-square box (rx=50, ry=25) must NOT collapse
    // to a circle of the larger radius; it is a circle of radius rx under a
    // y-scale of ry/rx = 0.5 about the center (finding M4).
    let pdf = export(
        &list(
            vec![DisplayItem::Fill {
                rect: Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
                radii: BorderRadii::ZERO,
                brush: Brush::RadialGradient(RadialGradient {
                    center: Point::new(50.0, 50.0),
                    radius: Size::new(50.0, 25.0),
                    stops: vec![
                        GradientStop {
                            offset: 0.0,
                            color: Color::rgb(255, 0, 0),
                        },
                        GradientStop {
                            offset: 1.0,
                            color: Color::rgb(0, 0, 255),
                        },
                    ],
                    extend: ExtendMode::Pad,
                }),
            }],
            ResourceTable::default(),
            600.0,
        ),
        &PdfOptions::default(),
    );
    let s = text(&pdf);
    // The content stream is uncompressed: the non-uniform y-scale cm appears
    // verbatim (`d = 0.5`, `f = center.y·(1−0.5) = 25`).
    assert!(s.contains("/ShadingType"), "radial shading present");
    assert!(
        s.contains("1 0 0 0.5 0 25 cm"),
        "elliptical y-scale transform in content: {s}"
    );
}

#[test]
fn image_streams_are_flate_compressed() {
    // A 64×64 solid image: uncompressed RGB is 12288 B and the SMask 4096 B.
    // Both streams must be FlateDecode-compressed (finding L4), so the whole
    // PDF is far smaller than the raw pixel data.
    let side = 64u32;
    let mut rgba = Vec::with_capacity((side * side * 4) as usize);
    for _ in 0..(side * side) {
        rgba.extend_from_slice(&[10, 20, 30, 255]);
    }
    let image = Arc::new(DecodedImage {
        id: ImageId(7),
        width: side,
        height: side,
        data: ImageData::Raster {
            rgba: Arc::new(rgba),
        },
    });
    let mut resources = ResourceTable::default();
    resources.add_image(image);
    let pdf = export(
        &list(
            vec![DisplayItem::Image {
                dst: Rect::from_xywh(0.0, 0.0, 64.0, 64.0),
                image: ImageId(7),
                tile: oxidepage_paint::TileMode::Stretch,
                radii: BorderRadii::ZERO,
            }],
            resources,
            600.0,
        ),
        &PdfOptions::default(),
    );
    let s = text(&pdf);
    // Both the RGB XObject and its alpha SMask carry the filter.
    assert!(
        s.matches("/FlateDecode").count() >= 2,
        "both image streams FlateDecode-compressed"
    );
    // Compressed well below the raw 12288 + 4096 = 16384 pixel bytes.
    assert!(
        pdf.len() < 16_384 / 2,
        "solid image compresses substantially (pdf is {} bytes)",
        pdf.len()
    );
}

/// Builds a PDF from a single linear-gradient fill over a 100×100 box with the
/// given axis, stops, and extend mode.
fn pdf_with_linear_extend(
    start: Point,
    end: Point,
    stops: Vec<GradientStop>,
    extend: ExtendMode,
) -> Vec<u8> {
    export(
        &list(
            vec![DisplayItem::Fill {
                rect: Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
                radii: BorderRadii::ZERO,
                brush: Brush::LinearGradient(LinearGradient {
                    start,
                    end,
                    stops,
                    extend,
                }),
            }],
            ResourceTable::default(),
            600.0,
        ),
        &PdfOptions::default(),
    )
}

#[test]
fn repeating_linear_gradient_tiles_stops_across_the_box() {
    // A red→blue axis over 20px, filling a 100px box. PDF shadings have no
    // native repeat, so `ExtendMode::Repeat` must tile the ramp across the box
    // (five periods) instead of clamping the ends like `Pad` (finding: honor the
    // extend mode). The two shadings must differ.
    let stops = vec![
        GradientStop {
            offset: 0.0,
            color: Color::rgb(255, 0, 0),
        },
        GradientStop {
            offset: 1.0,
            color: Color::rgb(0, 0, 255),
        },
    ];
    let pad = pdf_with_linear_extend(
        Point::new(0.0, 0.0),
        Point::new(20.0, 0.0),
        stops.clone(),
        ExtendMode::Pad,
    );
    let repeat = pdf_with_linear_extend(
        Point::new(0.0, 0.0),
        Point::new(20.0, 0.0),
        stops,
        ExtendMode::Repeat,
    );
    let ps = text(&pad);
    let rs = text(&repeat);
    // Pad clamps the end stops; the tiled repeat covers the box exactly, so it
    // does not extend beyond the widened axis.
    assert!(ps.contains("/Extend [true true]"), "pad clamps ends: {ps}");
    assert!(
        rs.contains("/Extend [false false]"),
        "repeat tiles, no clamp: {rs}"
    );
    // A 2-stop pad gradient is a single exponential; tiling stitches many.
    assert!(
        !ps.contains("/FunctionType 3"),
        "pad is a single exponential: {ps}"
    );
    assert!(
        rs.contains("/FunctionType 3"),
        "repeat stitches tiled segments: {rs}"
    );
    // 20px period over a 100px box → five red→blue ramps.
    assert!(
        rs.matches("/FunctionType 2").count() >= 5,
        "at least one ramp per period, got {}",
        rs.matches("/FunctionType 2").count()
    );
}

#[test]
fn reflecting_linear_gradient_also_tiles() {
    // Reflect mirrors alternate periods but, like Repeat, must tile (not clamp).
    let stops = vec![
        GradientStop {
            offset: 0.0,
            color: Color::rgb(255, 0, 0),
        },
        GradientStop {
            offset: 1.0,
            color: Color::rgb(0, 0, 255),
        },
    ];
    let reflect = pdf_with_linear_extend(
        Point::new(0.0, 0.0),
        Point::new(20.0, 0.0),
        stops,
        ExtendMode::Reflect,
    );
    let s = text(&reflect);
    assert!(s.contains("/Extend [false false]"), "reflect tiles: {s}");
    assert!(
        s.contains("/FunctionType 3"),
        "reflect stitches segments: {s}"
    );
}

/// Builds a PDF from a single 'X' glyph run of the embeddable Ahem font with the
/// given variation coords.
fn pdf_with_glyph_coords(coords: Vec<f32>) -> Vec<u8> {
    let blob = Blob::from(AHEM.to_vec());
    let font = FontId {
        blob: blob.id(),
        index: 0,
    };
    let gid = glyph_index(AHEM, 0, 'X').unwrap();
    let mut resources = ResourceTable::default();
    resources.add_font(FontResource {
        id: font,
        data: blob,
    });
    export(
        &list(
            vec![DisplayItem::GlyphRun {
                font,
                size: 20.0,
                color: Color::BLACK,
                normalized_coords: coords,
                glyphs: vec![PositionedGlyph {
                    id: gid,
                    x: 10.0,
                    y: 20.0,
                }],
                debug_text: Some("X".into()),
            }],
            resources,
            600.0,
        ),
        &PdfOptions::default(),
    )
}

#[test]
fn variable_font_instance_falls_back_to_outlines() {
    // Ahem is an embeddable TrueType, so with no variation coords its text is
    // embedded as a subset Type0/CIDFontType2 font. A non-empty
    // `normalized_coords` marks a variable-font instance the embedded-CID path
    // renders at the default master; it must instead fall back to baked vector
    // outlines (matching the raster backend, which varies its glyph cache).
    let default_master = text(&pdf_with_glyph_coords(Vec::new()));
    assert!(
        default_master.contains("/Type0") && default_master.contains("/FontFile2"),
        "without coords the glyph embeds as CID text: {default_master}"
    );

    let instance = text(&pdf_with_glyph_coords(vec![0.5]));
    assert!(
        !instance.contains("/Type0"),
        "variable instance is not embedded as CID text: {instance}"
    );
    assert!(
        !instance.contains("/FontFile2"),
        "variable instance embeds no font program: {instance}"
    );
    assert!(
        instance.contains("%%EOF"),
        "still a valid PDF (outlines emitted): {instance}"
    );
}

/// A red/blue overlapping-fill display list wrapped in one opacity/transform
/// layer.
fn layered_fills(opacity: f32, transform: Transform2D) -> Vec<DisplayItem> {
    vec![
        DisplayItem::PushLayer { opacity, transform },
        DisplayItem::Fill {
            rect: Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
            radii: BorderRadii::ZERO,
            brush: Brush::Solid(Color::rgb(255, 0, 0)),
        },
        DisplayItem::Fill {
            rect: Rect::from_xywh(25.0, 25.0, 50.0, 50.0),
            radii: BorderRadii::ZERO,
            brush: Brush::Solid(Color::rgb(0, 0, 255)),
        },
        DisplayItem::PopLayer,
    ]
}

#[test]
fn translucent_layer_becomes_a_transparency_group_form() {
    // A layer with opacity < 1 must be rendered into a Form XObject carrying a
    // /Group <</S /Transparency>> and drawn once under an ExtGState (ca/CA), so
    // overlapping children composite once at the layer's opacity instead of
    // darkening where they overlap (finding: true group opacity).
    let pdf = export(
        &list(
            layered_fills(0.5, Transform2D::IDENTITY),
            ResourceTable::default(),
            600.0,
        ),
        &PdfOptions::default(),
    );
    let s = text(&pdf);
    assert!(s.contains("/Subtype /Form"), "layer is a form XObject: {s}");
    assert!(s.contains("/Group"), "form has a group dict: {s}");
    assert!(
        s.contains("/S /Transparency"),
        "transparency group subtype: {s}"
    );
    // Drawn once under an opacity ExtGState (both ca and CA), then invoked.
    assert!(s.contains("/ca 0.5"), "non-stroking alpha: {s}");
    assert!(s.contains("/CA 0.5"), "stroking alpha: {s}");
    assert!(s.contains("Do"), "form invoked in the page content: {s}");
}

#[test]
fn opaque_layer_transform_emits_paired_cm_without_a_group() {
    // A PushLayer carrying only a CSS transform (opacity == 1) concatenates the
    // matrix onto the CTM inside a saved graphics state, with no transparency
    // group (that path had no test before this change).
    let pdf = export(
        &list(
            layered_fills(1.0, Transform2D::translation(10.0, 20.0)),
            ResourceTable::default(),
            600.0,
        ),
        &PdfOptions::default(),
    );
    let s = text(&pdf);
    // The content stream is uncompressed: the translate appears verbatim.
    assert!(
        s.contains("1 0 0 1 10 20 cm"),
        "layer transform in content: {s}"
    );
    // An opaque layer needs no compositing group. (A paginated export always
    // emits *one* form XObject — the document every page invokes — so the
    // absence being asserted is the transparency group, not the form.)
    assert!(
        !s.contains("/Group"),
        "opaque layer emits no transparency group: {s}"
    );
    // Paired save/restore around the transformed subtree.
    assert!(s.contains('q') && s.contains('Q'), "paired q/Q: {s}");
}
