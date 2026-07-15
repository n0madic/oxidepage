//! List markers (CSS Lists 3 §3): the bullet/number box a `display: list-item`
//! element generates.
//!
//! Two halves live here:
//!
//! 1. **Content** — [`marker_text`] turns a `list-style-type` plus the item's
//!    ordinal into the string the marker renders (`•`, `1.`, `iv.`, …).
//!    `construct` calls it while building the box tree.
//! 2. **Placement** — [`place_markers`] is the post-layout pass that positions
//!    every `list-style-position: outside` marker box.
//!
//! ### Why outside markers need their own pass
//!
//! An outside marker sits *outside* its list item's principal box, so it must
//! not take part in the item's flow. It is built as a child box of the item
//! carrying taffy `position: absolute` (which keeps it out of the flow in every
//! container type) and is then placed by hand: taffy would leave it at the
//! item's content-box origin, and — worse — an `InlineRoot` list item (the
//! common `<li>text</li>`) never lays out child boxes at all, since its children
//! are placed by parley from the `InlineBox` placeholders in its IFC and the
//! marker is deliberately not one of them. [`place_markers`] therefore measures
//! and positions the marker itself, which also makes the placement identical in
//! block, inline, flex and grid list items.
//!
//! The marker's right edge lands [`MARKER_GAP_EM`] before the item's content
//! edge, which right-aligns a run of numbers exactly the way browsers do (the
//! periods of `9.` and `10.` line up). Its box top is the item's content-box
//! top; because the marker inherits the item's font and line-height, its single
//! line box then shares the item's first baseline.
//!
//! ### Deliberate gaps (P6)
//!
//! * `list-style-image` is not implemented: the marker falls back to the
//!   `list-style-type`, which is also what CSS asks for when the image cannot be
//!   loaded.
//! * `::marker` is not a styleable pseudo-element. The marker box takes the list
//!   item's inherited style (font, color, line-height), so the default rendering
//!   is right; a `li::marker { color: red }` rule parses and cascades but hits
//!   nothing.
//! * The counter is derived structurally from the list (`<ol start>`,
//!   `<li value>`, `<ol reversed>`); `counter-reset`/`counter-increment` and
//!   `@counter-style` are not implemented.

use style::computed_values::list_style_position::T as ListStylePosition;
use style::counter_style::{CounterStyle, Symbol, SymbolsType};
use style::properties::ComputedValues;
use taffy::{
    AvailableSpace, LayoutInput, LayoutPartialTree as _, Line, RequestedAxis, RunMode, Size,
    SizingMode,
};

use crate::tree::{BoxId, LayoutTree};

/// Space between an outside marker's right edge and the list item's content
/// edge, in `em` of the marker's font size. Chosen to match the ~7–8 px browsers
/// leave at a 16 px default font.
const MARKER_GAP_EM: f32 = 0.5;

/// Where a list item's marker box goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MarkerPosition {
    /// The marker is the first inline-level content inside the item.
    Inside,
    /// The marker sits outside the item's principal box (the CSS default).
    Outside,
}

impl MarkerPosition {
    pub(crate) fn of(style: &ComputedValues) -> Self {
        match style.get_list().clone_list_style_position() {
            ListStylePosition::Inside => Self::Inside,
            ListStylePosition::Outside => Self::Outside,
        }
    }
}

/// The string a list item with computed values `style` renders as its marker at
/// `ordinal`, or `None` for `list-style-type: none`.
///
/// `trailing_space` appends the counter style's suffix space — an *inside*
/// marker separates itself from the item's text that way, while an *outside*
/// marker is separated by [`MARKER_GAP_EM`] and would otherwise carry a
/// trailing space that parley hangs off the end of the line.
#[must_use]
pub(crate) fn marker_text(
    style: &ComputedValues,
    ordinal: i32,
    trailing_space: bool,
) -> Option<String> {
    let list_style_type = style.get_list().clone_list_style_type();
    let (mut text, suffix) = match &list_style_type.0 {
        CounterStyle::None => return None,
        CounterStyle::Name(name) => named_counter(&name.0, ordinal),
        CounterStyle::String(s) => (s.to_string(), Suffix::Symbolic),
        CounterStyle::Symbols { ty, symbols } => {
            (anonymous_symbols(*ty, symbols, ordinal), Suffix::Symbolic)
        }
    };
    match suffix {
        Suffix::Alphanumeric => text.push('.'),
        Suffix::Symbolic => {}
    }
    if trailing_space {
        text.push(' ');
    }
    Some(text)
}

/// The suffix a counter style appends after its representation
/// (css-counter-styles-3 §3.1.6: `". "` by default, `" "` for the predefined
/// symbolic styles). The space is applied by [`marker_text`], which knows
/// whether the marker is inside or outside.
enum Suffix {
    /// `". "` — numeric, alphabetic and roman styles.
    Alphanumeric,
    /// `" "` — bullets, `symbols()`, and a `<string>` list-style-type.
    Symbolic,
}

/// The representation of `ordinal` in the predefined counter style `name`.
/// Unknown names (including `@counter-style` rules, which are not implemented)
/// fall back to `decimal`, as CSS requires of an unresolvable style.
fn named_counter(name: &style::Atom, ordinal: i32) -> (String, Suffix) {
    match &**name {
        "disc" => ("\u{2022}".to_owned(), Suffix::Symbolic),
        "circle" => ("\u{25e6}".to_owned(), Suffix::Symbolic),
        "square" => ("\u{25aa}".to_owned(), Suffix::Symbolic),
        "disclosure-open" => ("\u{25be}".to_owned(), Suffix::Symbolic),
        "disclosure-closed" => ("\u{25b8}".to_owned(), Suffix::Symbolic),
        "decimal-leading-zero" => (decimal_leading_zero(ordinal), Suffix::Alphanumeric),
        "lower-alpha" | "lower-latin" => (alphabetic(ordinal, LOWER_LATIN), Suffix::Alphanumeric),
        "upper-alpha" | "upper-latin" => (alphabetic(ordinal, UPPER_LATIN), Suffix::Alphanumeric),
        "lower-greek" => (alphabetic(ordinal, LOWER_GREEK), Suffix::Alphanumeric),
        "lower-roman" => (roman(ordinal, false), Suffix::Alphanumeric),
        "upper-roman" => (roman(ordinal, true), Suffix::Alphanumeric),
        // `decimal` and everything we do not implement.
        _ => (ordinal.to_string(), Suffix::Alphanumeric),
    }
}

const LOWER_LATIN: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z',
];
const UPPER_LATIN: &[char] = &[
    'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S',
    'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
];
const LOWER_GREEK: &[char] = &[
    'α', 'β', 'γ', 'δ', 'ε', 'ζ', 'η', 'θ', 'ι', 'κ', 'λ', 'μ', 'ν', 'ξ', 'ο', 'π', 'ρ', 'σ', 'τ',
    'υ', 'φ', 'χ', 'ψ', 'ω',
];

/// `decimal-leading-zero`: pad to two digits (the spec's `pad(2, "0")`).
fn decimal_leading_zero(ordinal: i32) -> String {
    let magnitude = ordinal.unsigned_abs();
    let sign = if ordinal < 0 { "-" } else { "" };
    if magnitude < 10 {
        format!("{sign}0{magnitude}")
    } else {
        format!("{sign}{magnitude}")
    }
}

/// A bijective base-*n* ("alphabetic") representation: `a`…`z`, `aa`…`az`, `ba`…
/// Outside the style's range (ordinal < 1) CSS falls back to `decimal`.
fn alphabetic(ordinal: i32, alphabet: &[char]) -> String {
    if ordinal < 1 {
        return ordinal.to_string();
    }
    let base = alphabet.len() as u32;
    let mut n = ordinal as u32;
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(alphabet[(n % base) as usize]);
        n /= base;
    }
    out.iter().rev().collect()
}

/// Roman numerals. Outside 1..=3999 CSS falls back to `decimal`.
fn roman(ordinal: i32, upper: bool) -> String {
    const DIGITS: [(u32, &str, &str); 13] = [
        (1000, "m", "M"),
        (900, "cm", "CM"),
        (500, "d", "D"),
        (400, "cd", "CD"),
        (100, "c", "C"),
        (90, "xc", "XC"),
        (50, "l", "L"),
        (40, "xl", "XL"),
        (10, "x", "X"),
        (9, "ix", "IX"),
        (5, "v", "V"),
        (4, "iv", "IV"),
        (1, "i", "I"),
    ];
    if !(1..=3999).contains(&ordinal) {
        return ordinal.to_string();
    }
    let mut n = ordinal as u32;
    let mut out = String::new();
    for (value, lower, uppercase) in DIGITS {
        while n >= value {
            n -= value;
            out.push_str(if upper { uppercase } else { lower });
        }
    }
    out
}

/// The representation of `ordinal` in an anonymous `symbols()` counter style
/// (css-counter-styles-3 §3.3).
fn anonymous_symbols(
    ty: SymbolsType,
    symbols: &style::counter_style::Symbols,
    ordinal: i32,
) -> String {
    let list: Vec<String> = symbols.0.iter().map(symbol_text).collect();
    if list.is_empty() {
        return ordinal.to_string();
    }
    let len = list.len() as i32;
    match ty {
        // Cyclic repeats the symbols forever, so every ordinal is in range.
        SymbolsType::Cyclic => list[(ordinal - 1).rem_euclid(len) as usize].clone(),
        SymbolsType::Fixed => {
            if (1..=len).contains(&ordinal) {
                list[(ordinal - 1) as usize].clone()
            } else {
                ordinal.to_string()
            }
        }
        SymbolsType::Symbolic => {
            if ordinal < 1 {
                return ordinal.to_string();
            }
            let index = ((ordinal - 1) % len) as usize;
            let repeats = ((ordinal - 1) / len + 1) as usize;
            list[index].repeat(repeats)
        }
        SymbolsType::Alphabetic => {
            if ordinal < 1 {
                return ordinal.to_string();
            }
            let base = list.len() as u32;
            let mut n = ordinal as u32;
            let mut parts = Vec::new();
            while n > 0 {
                n -= 1;
                parts.push(list[(n % base) as usize].as_str());
                n /= base;
            }
            parts.iter().rev().copied().collect()
        }
        SymbolsType::Numeric => {
            let base = list.len() as u32;
            if base < 2 || ordinal < 0 {
                return ordinal.to_string();
            }
            if ordinal == 0 {
                return list[0].clone();
            }
            let mut n = ordinal as u32;
            let mut parts = Vec::new();
            while n > 0 {
                parts.push(list[(n % base) as usize].as_str());
                n /= base;
            }
            parts.iter().rev().copied().collect()
        }
    }
}

fn symbol_text(symbol: &Symbol) -> String {
    match symbol {
        Symbol::String(s) => s.to_string(),
        Symbol::Ident(ident) => ident.0.to_string(),
    }
}

// === Placement ===

/// Measures and places every outside marker box (see the module docs). Runs
/// after taffy's layout pass and before rounding, so the marker's rounded
/// `final_layout` is produced by the same `round_layout` walk as every other
/// box.
pub(crate) fn place_markers(tree: &mut LayoutTree) {
    for index in 0..tree.box_count() {
        let box_id = BoxId(index as u32);
        if !tree.box_(box_id).is_outside_marker() {
            continue;
        }
        let Some(item) = tree.box_(box_id).parent else {
            continue;
        };

        // Shrink-to-fit: the marker is exactly as wide as its text.
        let output = tree.compute_child_layout(
            box_id.into(),
            LayoutInput {
                known_dimensions: Size::NONE,
                parent_size: Size::NONE,
                available_space: Size {
                    width: AvailableSpace::MaxContent,
                    height: AvailableSpace::MaxContent,
                },
                sizing_mode: SizingMode::InherentSize,
                axis: RequestedAxis::Both,
                run_mode: RunMode::PerformLayout,
                vertical_margins_are_collapsible: Line::FALSE,
            },
        );

        // The list item's content-box origin, relative to its border box —
        // which is the space a child box's location lives in.
        let item_layout = tree.box_(item).unrounded_layout;
        let content_x = item_layout.border.left + item_layout.padding.left;
        let content_y = item_layout.border.top + item_layout.padding.top;

        let marker = tree.box_mut(box_id);
        let gap = MARKER_GAP_EM * marker.font_size;
        marker.unrounded_layout.size = output.size;
        marker.unrounded_layout.location = taffy::Point {
            x: content_x - output.size.width - gap,
            y: content_y,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_leading_zero_pads_to_two_digits() {
        assert_eq!(decimal_leading_zero(1), "01");
        assert_eq!(decimal_leading_zero(9), "09");
        assert_eq!(decimal_leading_zero(10), "10");
        assert_eq!(decimal_leading_zero(100), "100");
        assert_eq!(decimal_leading_zero(-3), "-03");
    }

    #[test]
    fn alphabetic_is_bijective_base_26() {
        assert_eq!(alphabetic(1, LOWER_LATIN), "a");
        assert_eq!(alphabetic(26, LOWER_LATIN), "z");
        assert_eq!(alphabetic(27, LOWER_LATIN), "aa");
        assert_eq!(alphabetic(52, LOWER_LATIN), "az");
        assert_eq!(alphabetic(53, LOWER_LATIN), "ba");
        assert_eq!(alphabetic(3, UPPER_LATIN), "C");
        // Out of range: decimal fallback.
        assert_eq!(alphabetic(0, LOWER_LATIN), "0");
        assert_eq!(alphabetic(-1, LOWER_LATIN), "-1");
    }

    #[test]
    fn roman_numerals() {
        assert_eq!(roman(1, false), "i");
        assert_eq!(roman(4, false), "iv");
        assert_eq!(roman(9, true), "IX");
        assert_eq!(roman(14, false), "xiv");
        assert_eq!(roman(1990, true), "MCMXC");
        assert_eq!(roman(3999, false), "mmmcmxcix");
        // Out of range: decimal fallback.
        assert_eq!(roman(0, false), "0");
        assert_eq!(roman(4000, false), "4000");
    }
}
