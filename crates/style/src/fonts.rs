//! A no-op font metrics provider (design doc §10, ADR-0005).
//!
//! Phase 4 resolves *computed* values only; real font metrics (needed for `ex`,
//! `ch`, `ic` units and `font-size-adjust`) arrive with layout in Phase 5. Until
//! then this provider returns empty metrics and a fixed 16px (13px monospace)
//! base size, which is enough for the cascade.

use style::device::servo::FontMetricsProvider;
use style::font_metrics::FontMetrics;
use style::properties::style_structs::Font;
use style::values::computed::font::{GenericFontFamily, QueryFontMetricsFlags};
use style::values::computed::{CSSPixelLength, Length};

/// A [`FontMetricsProvider`] that reports no metrics and a fixed base size.
#[derive(Debug)]
pub struct NoopFontMetricsProvider;

impl FontMetricsProvider for NoopFontMetricsProvider {
    fn query_font_metrics(
        &self,
        _vertical: bool,
        _font: &Font,
        _base_size: CSSPixelLength,
        _flags: QueryFontMetricsFlags,
    ) -> FontMetrics {
        FontMetrics {
            x_height: None,
            zero_advance_measure: None,
            cap_height: None,
            ic_width: None,
            ascent: CSSPixelLength::new(0.0),
            script_percent_scale_down: None,
            script_script_percent_scale_down: None,
        }
    }

    fn base_size_for_generic(&self, generic: GenericFontFamily) -> Length {
        let px = if generic == GenericFontFamily::Monospace {
            13.0
        } else {
            16.0
        };
        Length::new(px)
    }
}
