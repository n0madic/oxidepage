//! Conversion functions from stylo computed values to parley text styles
//! (adapted from blitz-dom `stylo_to_parley.rs`).

use std::borrow::Cow;

use style::values::computed::Length;

use crate::tree::TextBrush;

/// Type aliases so we can refer to stylo types with nicer names.
pub(crate) mod stylo {
    pub(crate) use style::computed_values::text_wrap_mode::T as TextWrapMode;
    pub(crate) use style::computed_values::white_space_collapse::T as WhiteSpaceCollapse;
    pub(crate) use style::properties::ComputedValues;
    pub(crate) use style::values::computed::AlignmentBaseline;
    pub(crate) use style::values::computed::BaselineShift;
    pub(crate) use style::values::computed::OverflowWrap;
    pub(crate) use style::values::computed::WordBreak;
    pub(crate) use style::values::computed::font::FontStretch;
    pub(crate) use style::values::computed::font::FontStyle;
    pub(crate) use style::values::computed::font::FontVariationSettings;
    pub(crate) use style::values::computed::font::FontWeight;
    pub(crate) use style::values::computed::font::GenericFontFamily;
    pub(crate) use style::values::computed::font::LineHeight;
    pub(crate) use style::values::computed::font::SingleFontFamily;
    pub(crate) use style::values::generics::box_::BaselineShiftKeyword;
}

pub(crate) mod parley {
    pub(crate) use parley::FontVariation;
    pub(crate) use parley::fontique::QueryFamily;
    pub(crate) use parley::setting::*;
    pub(crate) use parley::style::*;
}

pub(crate) fn generic_font_family(input: stylo::GenericFontFamily) -> parley::GenericFamily {
    match input {
        stylo::GenericFontFamily::None => parley::GenericFamily::SansSerif,
        stylo::GenericFontFamily::Serif => parley::GenericFamily::Serif,
        stylo::GenericFontFamily::SansSerif => parley::GenericFamily::SansSerif,
        stylo::GenericFontFamily::Monospace => parley::GenericFamily::Monospace,
        stylo::GenericFontFamily::Cursive => parley::GenericFamily::Cursive,
        stylo::GenericFontFamily::Fantasy => parley::GenericFamily::Fantasy,
        stylo::GenericFontFamily::SystemUi => parley::GenericFamily::SystemUi,
    }
}

pub(crate) fn query_font_family(input: &stylo::SingleFontFamily) -> parley::QueryFamily<'_> {
    match input {
        stylo::SingleFontFamily::FamilyName(name) => {
            'ret: {
                let name = name.name.as_ref();

                // Legacy web compatibility
                #[cfg(target_vendor = "apple")]
                if name == "-apple-system" {
                    break 'ret parley::QueryFamily::Generic(parley::GenericFamily::SystemUi);
                }
                #[cfg(target_os = "macos")]
                if name == "BlinkMacSystemFont" {
                    break 'ret parley::QueryFamily::Generic(parley::GenericFamily::SystemUi);
                }

                break 'ret parley::QueryFamily::Named(name);
            }
        }
        stylo::SingleFontFamily::Generic(generic) => {
            parley::QueryFamily::Generic(self::generic_font_family(*generic))
        }
    }
}

pub(crate) fn font_weight(input: stylo::FontWeight) -> parley::FontWeight {
    parley::FontWeight::new(input.value())
}

pub(crate) fn font_width(input: stylo::FontStretch) -> parley::FontWidth {
    parley::FontWidth::from_percentage(input.0.to_float())
}

pub(crate) fn font_style(input: stylo::FontStyle) -> parley::FontStyle {
    match input {
        stylo::FontStyle::NORMAL => parley::FontStyle::Normal,
        stylo::FontStyle::ITALIC => parley::FontStyle::Italic,
        val => parley::FontStyle::Oblique(Some(val.oblique_degrees())),
    }
}

pub(crate) fn font_variations(input: &stylo::FontVariationSettings) -> Vec<parley::FontVariation> {
    input
        .0
        .iter()
        .map(|v| parley::FontVariation {
            tag: parley::Tag::from_bytes(v.tag.0.to_be_bytes()),
            value: v.value,
        })
        .collect()
}

pub(crate) fn white_space_collapse(input: stylo::WhiteSpaceCollapse) -> parley::WhiteSpaceCollapse {
    match input {
        stylo::WhiteSpaceCollapse::Collapse => parley::WhiteSpaceCollapse::Collapse,
        stylo::WhiteSpaceCollapse::Preserve => parley::WhiteSpaceCollapse::Preserve,
        stylo::WhiteSpaceCollapse::PreserveBreaks => parley::WhiteSpaceCollapse::PreserveBreaks,
        stylo::WhiteSpaceCollapse::BreakSpaces => parley::WhiteSpaceCollapse::BreakSpaces,
    }
}

/// Converts the `vertical-align` longhands (`alignment-baseline` +
/// `baseline-shift`) to parley's equivalent. `line_height` is the element's
/// own used line-height in px, which a percentage shift resolves against
/// (CSS 2.1 §10.8.1).
pub(crate) fn vertical_align(
    style: &stylo::ComputedValues,
    line_height: f32,
) -> parley::VerticalAlign {
    let box_styles = style.get_box();
    let alignment = match box_styles.alignment_baseline {
        stylo::AlignmentBaseline::Baseline => parley::AlignmentBaseline::Baseline,
        stylo::AlignmentBaseline::TextBottom => parley::AlignmentBaseline::TextBottom,
        stylo::AlignmentBaseline::Middle => parley::AlignmentBaseline::Middle,
        stylo::AlignmentBaseline::TextTop => parley::AlignmentBaseline::TextTop,
    };
    let shift = match &box_styles.baseline_shift {
        stylo::BaselineShift::Keyword(keyword) => match keyword {
            stylo::BaselineShiftKeyword::Sub => parley::BaselineShift::Sub,
            stylo::BaselineShiftKeyword::Super => parley::BaselineShift::Super,
            stylo::BaselineShiftKeyword::Top => parley::BaselineShift::Top,
            stylo::BaselineShiftKeyword::Bottom => parley::BaselineShift::Bottom,
            // css-inline-3's `center` (line-relative centring) has no parley
            // counterpart; it stays on the baseline.
            stylo::BaselineShiftKeyword::Center => parley::BaselineShift::Length(0.0),
        },
        stylo::BaselineShift::Length(length) => {
            parley::BaselineShift::Length(length.resolve(Length::new(line_height)).px())
        }
    };
    parley::VerticalAlign::new(alignment, shift)
}

/// `line-height: normal` factor. Matches blitz-dom's 1.2× approximation
/// rather than per-font metrics: font-metric-relative line heights would
/// vary across platforms (and invalidate the committed WPT expectations),
/// so v1 keeps the fixed factor everywhere it is needed.
pub(crate) const NORMAL_LINE_HEIGHT: f32 = 1.2;

/// Converts a computed line-height to CSS px, given the used font size.
pub(crate) fn resolve_line_height(line_height: parley::LineHeight, font_size: f32) -> f32 {
    match line_height {
        parley::LineHeight::FontSizeRelative(relative) => relative * font_size,
        parley::LineHeight::Absolute(absolute) => absolute,
        parley::LineHeight::MetricsRelative(relative) => relative * font_size,
    }
}

/// Converts a computed `line-height` to parley's representation.
pub(crate) fn line_height(style: &stylo::ComputedValues) -> parley::LineHeight {
    match style.get_font().line_height {
        stylo::LineHeight::Normal => parley::LineHeight::FontSizeRelative(NORMAL_LINE_HEIGHT),
        stylo::LineHeight::Number(num) => parley::LineHeight::FontSizeRelative(num.0),
        stylo::LineHeight::Length(value) => parley::LineHeight::Absolute(value.0.px()),
    }
}

/// An element's own used `line-height` in CSS px.
pub(crate) fn style_line_height(style: &stylo::ComputedValues) -> f32 {
    resolve_line_height(
        line_height(style),
        style.get_font().font_size.used_size.0.px(),
    )
}

/// Builds the parley text style for a text span owned by `brush`'s node.
pub(crate) fn style(
    brush: TextBrush,
    style: &stylo::ComputedValues,
) -> parley::TextStyle<'static, 'static, TextBrush> {
    let font_styles = style.get_font();
    let itext_styles = style.get_inherited_text();

    // Convert font size and line height
    let font_size = font_styles.font_size.used_size.0.px();
    let line_height = self::line_height(style);

    let vertical_align = self::vertical_align(style, resolve_line_height(line_height, font_size));

    let letter_spacing = itext_styles
        .letter_spacing
        .0
        .resolve(Length::new(font_size))
        .px();

    let word_spacing = itext_styles
        .word_spacing
        .resolve(Length::new(font_size))
        .px();

    // Convert Bold/Italic
    let font_weight = self::font_weight(font_styles.font_weight);
    let font_style = self::font_style(font_styles.font_style);
    let font_width = self::font_width(font_styles.font_stretch);
    let font_variations = self::font_variations(&font_styles.font_variation_settings);

    // Convert font family
    let families: Vec<_> = font_styles
        .font_family
        .families
        .list
        .iter()
        .map(|family| match family {
            stylo::SingleFontFamily::FamilyName(name) => {
                'ret: {
                    let name = name.name.as_ref();

                    // Legacy web compatibility
                    #[cfg(target_vendor = "apple")]
                    if name == "-apple-system" {
                        break 'ret parley::FontFamilyName::Generic(
                            parley::GenericFamily::SystemUi,
                        );
                    }
                    #[cfg(target_os = "macos")]
                    if name == "BlinkMacSystemFont" {
                        break 'ret parley::FontFamilyName::Generic(
                            parley::GenericFamily::SystemUi,
                        );
                    }

                    break 'ret parley::FontFamilyName::Named(Cow::Owned(name.to_string()));
                }
            }
            stylo::SingleFontFamily::Generic(generic) => {
                parley::FontFamilyName::Generic(self::generic_font_family(*generic))
            }
        })
        .collect();

    // Wrapping and breaking
    let word_break = match itext_styles.word_break {
        stylo::WordBreak::Normal => parley::WordBreak::Normal,
        stylo::WordBreak::BreakAll => parley::WordBreak::BreakAll,
        stylo::WordBreak::KeepAll => parley::WordBreak::KeepAll,
    };
    let overflow_wrap = match itext_styles.overflow_wrap {
        stylo::OverflowWrap::Normal => parley::OverflowWrap::Normal,
        stylo::OverflowWrap::BreakWord => parley::OverflowWrap::BreakWord,
        stylo::OverflowWrap::Anywhere => parley::OverflowWrap::Anywhere,
    };
    let text_wrap_mode = match itext_styles.text_wrap_mode {
        stylo::TextWrapMode::Wrap => parley::TextWrapMode::Wrap,
        stylo::TextWrapMode::Nowrap => parley::TextWrapMode::NoWrap,
    };

    parley::TextStyle {
        font_family: parley::FontFamily::List(Cow::Owned(families)),
        font_size,
        font_width,
        font_style,
        font_weight,
        font_variations: parley::FontVariations::List(Cow::Owned(font_variations)),
        font_features: parley::FontFeatures::List(Cow::Borrowed(&[])),
        locale: Default::default(),
        line_height,
        word_spacing,
        letter_spacing,
        text_wrap_mode,
        overflow_wrap,
        word_break,
        white_space_collapse: self::white_space_collapse(itext_styles.white_space_collapse),
        // stylo's servo engine does not ship `line-break`, so every span
        // keeps the UAX #14 default.
        line_break: Default::default(),
        vertical_align,

        // Contains the owning NodeId
        brush,

        // Decoration styles don't affect layout; paint (Phase 6) reads them
        // from the computed style via the brush's NodeId instead.
        has_underline: Default::default(),
        underline_offset: Default::default(),
        underline_size: Default::default(),
        underline_brush: Default::default(),
        has_strikethrough: Default::default(),
        strikethrough_offset: Default::default(),
        strikethrough_size: Default::default(),
        strikethrough_brush: Default::default(),
    }
}

/// Converts `text-align` to a parley alignment.
pub(crate) fn text_align(style: &stylo::ComputedValues) -> ::parley::layout::Alignment {
    use ::parley::layout::Alignment;
    use style::values::specified::TextAlignKeyword;

    match style.clone_text_align() {
        TextAlignKeyword::Start => Alignment::Start,
        TextAlignKeyword::Left => Alignment::Left,
        TextAlignKeyword::Right => Alignment::Right,
        TextAlignKeyword::Center => Alignment::Center,
        TextAlignKeyword::Justify => Alignment::Justify,
        TextAlignKeyword::End => Alignment::End,
        TextAlignKeyword::MozCenter => Alignment::Center,
        TextAlignKeyword::MozLeft => Alignment::Left,
        TextAlignKeyword::MozRight => Alignment::Right,
    }
}
