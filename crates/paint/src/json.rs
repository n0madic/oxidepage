//! Hand-written JSON serialization for the display list.
//!
//! Serde is deliberately avoided: goldens must be byte-stable and reviewable,
//! so floats are fixed to two decimals (and `-0.00` normalized to `0.00`),
//! colors are `#rrggbbaa`, and each display item prints on its own line.

use oxidepage_base::{Point, Rect, Size, Transform2D};

use crate::display_list::{
    BorderEdge, BorderRadii, BorderStyle, Brush, Color, DisplayItem, DisplayList, GradientStop,
    PositionedGlyph, TileMode,
};

/// Formats a float with two decimals, normalizing negative zero. Non-finite
/// values (`inf`/`-inf`/`NaN`) would render as tokens that are not valid JSON
/// numbers, so they collapse to `0.00` to keep the dump machine-parseable.
fn f(v: f32) -> String {
    if !v.is_finite() {
        return "0.00".to_string();
    }
    let s = format!("{v:.2}");
    if s == "-0.00" { "0.00".to_string() } else { s }
}

fn point(p: &Point) -> String {
    format!("[{}, {}]", f(p.x), f(p.y))
}

fn size(s: &Size) -> String {
    format!("[{}, {}]", f(s.width), f(s.height))
}

fn rect(r: &Rect) -> String {
    format!(
        "[{}, {}, {}, {}]",
        f(r.origin.x),
        f(r.origin.y),
        f(r.size.width),
        f(r.size.height)
    )
}

fn color(c: &Color) -> String {
    format!("\"#{:02x}{:02x}{:02x}{:02x}\"", c.r, c.g, c.b, c.a)
}

fn radii(r: &BorderRadii) -> String {
    if r.is_zero() {
        return "null".to_string();
    }
    format!(
        "[{}, {}, {}, {}, {}, {}, {}, {}]",
        f(r.top_left.width),
        f(r.top_left.height),
        f(r.top_right.width),
        f(r.top_right.height),
        f(r.bottom_right.width),
        f(r.bottom_right.height),
        f(r.bottom_left.width),
        f(r.bottom_left.height),
    )
}

fn stops(stops: &[GradientStop]) -> String {
    let inner: Vec<String> = stops
        .iter()
        .map(|s| {
            format!(
                "{{ \"offset\": {}, \"color\": {} }}",
                f(s.offset),
                color(&s.color)
            )
        })
        .collect();
    format!("[{}]", inner.join(", "))
}

fn extend(e: crate::display_list::ExtendMode) -> &'static str {
    match e {
        crate::display_list::ExtendMode::Pad => "pad",
        crate::display_list::ExtendMode::Repeat => "repeat",
        crate::display_list::ExtendMode::Reflect => "reflect",
    }
}

fn brush(b: &Brush) -> String {
    match b {
        Brush::Solid(c) => format!("{{ \"solid\": {} }}", color(c)),
        Brush::LinearGradient(g) => format!(
            "{{ \"linear\": {{ \"start\": {}, \"end\": {}, \"extend\": \"{}\", \"stops\": {} }} }}",
            point(&g.start),
            point(&g.end),
            extend(g.extend),
            stops(&g.stops),
        ),
        Brush::RadialGradient(g) => format!(
            "{{ \"radial\": {{ \"center\": {}, \"radius\": {}, \"extend\": \"{}\", \"stops\": {} }} }}",
            point(&g.center),
            size(&g.radius),
            extend(g.extend),
            stops(&g.stops),
        ),
    }
}

fn border_style(s: BorderStyle) -> &'static str {
    match s {
        BorderStyle::None => "none",
        BorderStyle::Hidden => "hidden",
        BorderStyle::Solid => "solid",
        BorderStyle::Double => "double",
        BorderStyle::Dotted => "dotted",
        BorderStyle::Dashed => "dashed",
        BorderStyle::Groove => "groove",
        BorderStyle::Ridge => "ridge",
        BorderStyle::Inset => "inset",
        BorderStyle::Outset => "outset",
    }
}

fn edge(e: &BorderEdge) -> String {
    format!(
        "{{ \"width\": {}, \"color\": {}, \"style\": \"{}\" }}",
        f(e.width),
        color(&e.color),
        border_style(e.style)
    )
}

fn edges(edges: &[BorderEdge; 4]) -> String {
    format!(
        "[{}, {}, {}, {}]",
        edge(&edges[0]),
        edge(&edges[1]),
        edge(&edges[2]),
        edge(&edges[3])
    )
}

fn tile_mode(t: TileMode) -> &'static str {
    match t {
        TileMode::Stretch => "stretch",
        TileMode::Repeat => "repeat",
        TileMode::RepeatX => "repeat-x",
        TileMode::RepeatY => "repeat-y",
    }
}

fn glyphs(glyphs: &[PositionedGlyph]) -> String {
    let inner: Vec<String> = glyphs
        .iter()
        .map(|g| {
            format!(
                "{{ \"id\": {}, \"x\": {}, \"y\": {} }}",
                g.id,
                f(g.x),
                f(g.y)
            )
        })
        .collect();
    format!("[{}]", inner.join(", "))
}

fn coords(coords: &[f32]) -> String {
    let inner: Vec<String> = coords.iter().map(|&c| f(c)).collect();
    format!("[{}]", inner.join(", "))
}

/// Escapes a string for embedding in a JSON double-quoted literal.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn item(
    item: &DisplayItem,
    font_ordinal: &std::collections::HashMap<u64, usize>,
    image_ordinal: &std::collections::HashMap<u64, usize>,
) -> String {
    match item {
        DisplayItem::Fill {
            rect: r,
            radii: rad,
            brush: b,
        } => format!(
            "{{ \"type\": \"Fill\", \"rect\": {}, \"radii\": {}, \"brush\": {} }}",
            rect(r),
            radii(rad),
            brush(b)
        ),
        DisplayItem::Border {
            rect: r,
            radii: rad,
            edges: e,
        } => format!(
            "{{ \"type\": \"Border\", \"rect\": {}, \"radii\": {}, \"edges\": {} }}",
            rect(r),
            radii(rad),
            edges(e)
        ),
        DisplayItem::Image {
            dst,
            image,
            tile,
            radii: rad,
        } => {
            // Reference the image by its resource ordinal (image ids are a
            // per-process counter and unstable across runs).
            let ordinal = image_ordinal.get(&image.0).copied().unwrap_or(usize::MAX);
            format!(
                "{{ \"type\": \"Image\", \"dst\": {}, \"image\": {}, \"tile\": \"{}\", \"radii\": {} }}",
                rect(dst),
                ordinal as i64,
                tile_mode(*tile),
                radii(rad)
            )
        }
        DisplayItem::GlyphRun {
            font,
            size: sz,
            color: c,
            normalized_coords,
            glyphs: gs,
            debug_text,
        } => {
            let text = match debug_text {
                Some(t) => json_string(t),
                None => "null".to_string(),
            };
            // Reference the font by its resource ordinal, not the raw blob id:
            // blob ids come from a per-process counter and are unstable across
            // runs, which would break goldens.
            let ordinal = font_ordinal.get(&font.blob).copied().unwrap_or(usize::MAX);
            format!(
                "{{ \"type\": \"GlyphRun\", \"font\": {}, \"face\": {}, \"size\": {}, \"color\": {}, \"coords\": {}, \"glyphs\": {}, \"text\": {} }}",
                ordinal as i64,
                font.index,
                f(*sz),
                color(c),
                coords(normalized_coords),
                glyphs(gs),
                text
            )
        }
        DisplayItem::PushClip {
            rect: r,
            radii: rad,
        } => format!(
            "{{ \"type\": \"PushClip\", \"rect\": {}, \"radii\": {} }}",
            rect(r),
            radii(rad)
        ),
        DisplayItem::PopClip => "{ \"type\": \"PopClip\" }".to_string(),
        DisplayItem::PushLayer { opacity, transform } => format!(
            "{{ \"type\": \"PushLayer\", \"opacity\": {}, \"transform\": {} }}",
            f(*opacity),
            transform_json(transform)
        ),
        DisplayItem::PopLayer => "{ \"type\": \"PopLayer\" }".to_string(),
        DisplayItem::PushViewportAnchor => "{ \"type\": \"PushViewportAnchor\" }".to_string(),
        DisplayItem::PopViewportAnchor => "{ \"type\": \"PopViewportAnchor\" }".to_string(),
    }
}

fn transform_json(t: &Transform2D) -> String {
    format!(
        "[{}, {}, {}, {}, {}, {}]",
        f(t.a),
        f(t.b),
        f(t.c),
        f(t.d),
        f(t.tx),
        f(t.ty)
    )
}

/// Appends a JSON array of pre-rendered `entries`: each on its own line
/// prefixed with `entry_indent`, comma-separated, the closing `]` prefixed
/// with `close_indent`; an empty array collapses to `[]`. Keeps the three
/// display-list arrays (items, fonts, images) byte-identical (goldens depend
/// on the exact formatting).
fn push_json_array(out: &mut String, entries: &[String], entry_indent: &str, close_indent: &str) {
    out.push('[');
    if entries.is_empty() {
        out.push(']');
        return;
    }
    out.push('\n');
    for (i, entry) in entries.iter().enumerate() {
        out.push_str(entry_indent);
        out.push_str(entry);
        if i + 1 < entries.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(close_indent);
    out.push(']');
}

/// Serializes `list` to the golden JSON format.
pub(crate) fn display_list_to_json(list: &DisplayList) -> String {
    // Map each font blob id to its ordinal in the resource table, so glyph
    // runs reference fonts by stable position rather than the unstable blob id.
    let font_ordinal: std::collections::HashMap<u64, usize> = list
        .resources
        .fonts
        .iter()
        .enumerate()
        .map(|(i, f)| (f.id.blob, i))
        .collect();
    let image_ordinal: std::collections::HashMap<u64, usize> = list
        .resources
        .images
        .iter()
        .enumerate()
        .map(|(i, img)| (img.id.0, i))
        .collect();

    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"viewport\": {},\n", size(&list.viewport)));
    out.push_str(&format!(
        "  \"content_size\": {},\n",
        size(&list.content_size)
    ));

    let item_entries: Vec<String> = list
        .items
        .iter()
        .map(|it| item(it, &font_ordinal, &image_ordinal))
        .collect();
    out.push_str("  \"items\": ");
    push_json_array(&mut out, &item_entries, "    ", "  ");
    out.push_str(",\n");

    out.push_str("  \"resources\": {\n");
    // The face index; the array position is the stable resource id referenced
    // by glyph runs' "font" field.
    let font_entries: Vec<String> = list
        .resources
        .fonts
        .iter()
        .map(|font| format!("{{ \"face\": {} }}", font.id.index))
        .collect();
    out.push_str("    \"fonts\": ");
    push_json_array(&mut out, &font_entries, "      ", "    ");
    out.push_str(",\n");
    // The decoded size; the array position is the stable resource id referenced
    // by image items' "image" field.
    let image_entries: Vec<String> = list
        .resources
        .images
        .iter()
        .map(|image| format!("{{ \"size\": [{}, {}] }}", image.width, image.height))
        .collect();
    out.push_str("    \"images\": ");
    push_json_array(&mut out, &image_entries, "      ", "    ");
    out.push('\n');
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_list::*;
    use oxidepage_base::Size;

    #[test]
    fn json_dump_is_stable() {
        let list = DisplayList {
            viewport: Size::new(800.0, 600.0),
            content_size: Size::new(800.0, 600.0),
            items: vec![
                DisplayItem::Fill {
                    rect: Rect::from_xywh(0.0, 0.0, 800.0, 600.0),
                    radii: BorderRadii::ZERO,
                    brush: Brush::Solid(Color::WHITE),
                },
                DisplayItem::Fill {
                    rect: Rect::from_xywh(10.0, 20.0, 100.0, 50.0),
                    radii: BorderRadii::uniform(4.0),
                    brush: Brush::Solid(Color::rgba(255, 0, 0, 128)),
                },
            ],
            resources: ResourceTable::default(),
        };

        let expected = "\
{
  \"viewport\": [800.00, 600.00],
  \"content_size\": [800.00, 600.00],
  \"items\": [
    { \"type\": \"Fill\", \"rect\": [0.00, 0.00, 800.00, 600.00], \"radii\": null, \"brush\": { \"solid\": \"#ffffffff\" } },
    { \"type\": \"Fill\", \"rect\": [10.00, 20.00, 100.00, 50.00], \"radii\": [4.00, 4.00, 4.00, 4.00, 4.00, 4.00, 4.00, 4.00], \"brush\": { \"solid\": \"#ff000080\" } }
  ],
  \"resources\": {
    \"fonts\": [],
    \"images\": []
  }
}
";
        assert_eq!(list.to_json(), expected);

        // The dump is deterministic.
        assert_eq!(list.to_json(), list.to_json());
    }

    #[test]
    fn negative_zero_normalized() {
        assert_eq!(f(-0.0), "0.00");
        assert_eq!(f(-0.001), "0.00");
        assert_eq!(f(-1.5), "-1.50");
    }

    #[test]
    fn non_finite_floats_normalized() {
        // `inf`/`-inf`/`NaN` would otherwise serialize as invalid JSON tokens.
        assert_eq!(f(f32::INFINITY), "0.00");
        assert_eq!(f(f32::NEG_INFINITY), "0.00");
        assert_eq!(f(f32::NAN), "0.00");
    }

    #[test]
    fn non_finite_coordinates_serialize_to_valid_json() {
        // A display item carrying inf/NaN geometry still dumps to JSON that a
        // strict parser accepts (the goldens / `dump --format display-list`).
        let list = DisplayList {
            viewport: Size::new(f32::INFINITY, f32::NAN),
            content_size: Size::new(800.0, 600.0),
            items: vec![DisplayItem::Fill {
                rect: Rect::from_xywh(f32::NEG_INFINITY, f32::NAN, f32::INFINITY, 10.0),
                radii: BorderRadii::ZERO,
                brush: Brush::Solid(Color::WHITE),
            }],
            resources: ResourceTable::default(),
        };
        let json = list.to_json();
        assert!(
            !json.contains("inf") && !json.contains("NaN"),
            "no non-finite tokens: {json}"
        );
        // The offending coordinates collapsed to 0.00.
        assert!(json.contains("[0.00, 0.00, 0.00, 10.00]"), "{json}");
    }
}
