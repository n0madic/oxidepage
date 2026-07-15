//! `@font-face` rule discovery (Phase 7, WP-B, ADR-0008).
//!
//! [`StyleEngine::font_faces`](crate::StyleEngine::font_faces) walks the
//! author stylesheets' *effective* rules (which descends into `@media`,
//! `@supports`, `@layer`, and `@import`, honoring media evaluation) and lifts
//! each `@font-face` rule into a backend-neutral [`FontFaceInfo`]: the family
//! name it binds, its ordered `src:` list, and the width/style/weight/range
//! descriptors the font loader needs to register and match the face.
//!
//! The descriptor ranges are collapsed to the single values fontique's
//! `FontInfoOverride` accepts; `unicode-range` is captured but, per the v1
//! limitations, only approximated at match time (ADR-0008).

use style::font_face::{
    ComputedFontStyleDescriptor, FontFaceRule, FontFaceSourceFormat, FontFaceSourceFormatKeyword,
    FontStyle, Source,
};

/// The visual slope requested by a resolved `@font-face` `font-style`
/// descriptor (angles in CSS degrees).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FontFaceStyle {
    Normal,
    Italic,
    Oblique(f32),
}

/// A hint at a `src:` entry's font format, from its `format(...)` function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontFormatHint {
    Woff2,
    Woff,
    Truetype,
    Opentype,
    /// Any other keyword/string (svg, embedded-opentype, collection, …).
    Other,
}

/// One `src:` entry of an `@font-face` rule.
#[derive(Clone, Debug)]
pub struct FontFaceSource {
    /// Absolute URL of a `url(...)` source (resolved against the sheet's base).
    pub url: Option<String>,
    /// Family name of a `local(...)` source.
    pub local: Option<String>,
    /// The `format(...)` hint, if present.
    pub format: Option<FontFormatHint>,
}

/// A resolved `@font-face` rule, ready for the page font loader (WP-D) to fetch
/// and the layout font system (WP-C) to register.
#[derive(Clone, Debug)]
pub struct FontFaceInfo {
    /// The CSS `font-family` the face binds to.
    pub family: String,
    /// The `src:` sources, in declaration (preference) order.
    pub sources: Vec<FontFaceSource>,
    /// The `unicode-range` descriptor as inclusive `(start, end)` codepoint
    /// pairs, if specified.
    pub unicode_range: Option<Vec<(u32, u32)>>,
    /// The `font-weight` descriptor range `(min, max)`; `(400, 400)` by default.
    pub weight: (f32, f32),
    /// The `font-style` descriptor.
    pub style: FontFaceStyle,
    /// The `font-stretch` descriptor as a percentage (`100` = normal).
    pub stretch: f32,
}

/// Builds a [`FontFaceInfo`] from a parsed `@font-face` rule, or `None` when it
/// has no usable `font-family` / `src` (a rule that can never resolve).
pub(crate) fn from_rule(rule: &FontFaceRule) -> Option<FontFaceInfo> {
    let d = &rule.descriptors;

    let family = d.font_family.as_ref()?.name.to_string();
    let source_list = d.src.as_ref()?;
    let sources: Vec<FontFaceSource> = source_list.0.iter().map(convert_source).collect();
    if sources.is_empty() {
        return None;
    }

    // A descriptor whose bounds are an unresolvable `calc()` computes to `None`;
    // fall back to the initial value rather than dropping the whole face.
    let weight = d
        .font_weight
        .as_ref()
        .and_then(|w| Some((w.0.compute()?.value(), w.1.compute()?.value())))
        .unwrap_or((400.0, 400.0));
    let style = match d.font_style.as_ref().and_then(FontStyle::compute) {
        None => FontFaceStyle::Normal,
        Some(ComputedFontStyleDescriptor::Italic) => FontFaceStyle::Italic,
        // `normal` computes to `oblique 0deg 0deg`; only a fully-zero slope
        // range is upright. A range with a non-zero endpoint (e.g. `oblique
        // 0deg 20deg`) is a real oblique face and must not collapse to
        // Normal — the downstream `WebFontAttrs`/fontique style is a single
        // angle, so keep the first non-zero endpoint of the range.
        Some(ComputedFontStyleDescriptor::Oblique(min, max)) => {
            match (min.to_float(), max.to_float()) {
                (0.0, 0.0) => FontFaceStyle::Normal,
                (0.0, max) => FontFaceStyle::Oblique(max),
                (min, _) => FontFaceStyle::Oblique(min),
            }
        }
    };
    let stretch = d
        .font_stretch
        .as_ref()
        .map_or(100.0, |s| stretch_percentage(&s.0));
    let unicode_range = d
        .unicode_range
        .as_ref()
        .map(|ranges| ranges.iter().map(|r| (r.start, r.end)).collect());

    Some(FontFaceInfo {
        family,
        sources,
        unicode_range,
        weight,
        style,
        stretch,
    })
}

fn convert_source(source: &Source) -> FontFaceSource {
    match source {
        Source::Url(url_source) => FontFaceSource {
            url: url_source.url.url().map(|u| u.as_str().to_owned()),
            local: None,
            format: url_source.format_hint.as_ref().map(convert_format),
        },
        Source::Local(name) => FontFaceSource {
            url: None,
            local: Some(name.name.to_string()),
            format: None,
        },
    }
}

fn convert_format(format: &FontFaceSourceFormat) -> FontFormatHint {
    use FontFaceSourceFormatKeyword as Kw;
    match format {
        FontFaceSourceFormat::Keyword(kw) => match kw {
            Kw::Woff2 => FontFormatHint::Woff2,
            Kw::Woff => FontFormatHint::Woff,
            Kw::Truetype | Kw::Collection => FontFormatHint::Truetype,
            Kw::Opentype => FontFormatHint::Opentype,
            _ => FontFormatHint::Other,
        },
        FontFaceSourceFormat::String(s) => match s.to_ascii_lowercase().as_str() {
            "woff2" => FontFormatHint::Woff2,
            "woff" => FontFormatHint::Woff,
            "truetype" => FontFormatHint::Truetype,
            "opentype" => FontFormatHint::Opentype,
            _ => FontFormatHint::Other,
        },
    }
}

/// Collapses a specified `@font-face` `font-stretch` component to a percentage
/// (`100` = normal), mirroring stylo's own `FontStretchRange::compute`.
fn stretch_percentage(s: &style::values::specified::font::FontStretch) -> f32 {
    use style::values::computed::font::FontStretch as ComputedStretch;
    use style::values::specified::font::FontStretch as SpecifiedStretch;
    let computed = match s {
        SpecifiedStretch::Keyword(kw) => kw.compute(),
        // An unresolvable `calc()` percentage has no value; treat it as normal.
        SpecifiedStretch::Stretch(p) => {
            p.0.get()
                .map_or(ComputedStretch::NORMAL, ComputedStretch::from_percentage)
        }
        SpecifiedStretch::System(..) => ComputedStretch::NORMAL,
    };
    computed.0.to_float()
}
