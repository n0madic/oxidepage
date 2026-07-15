//! Font loading and shaping contexts (adapted from blitz-dom's document
//! setup).
//!
//! [`FontSystem`] owns the parley [`FontContext`] (font collection + source
//! cache) behind an `Arc<Mutex<…>>` — the same handle is shared with the
//! style engine's font-metrics provider (WP-H) — plus the [`LayoutContext`]
//! used for shaping inline layouts.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, PoisonError};

use oxidepage_style::{FontFaceInfo, FontFaceStyle};
use parley::fontique::{
    Blob, Collection, CollectionOptions, FontInfoOverride, FontStyle, FontWeight, FontWidth,
    GenericFamily, SourceCache,
};
use parley::{FontContext, LayoutContext};

use crate::tree::TextBrush;

/// What [`FontSystem::register_web_font`] did with a downloaded `@font-face`
/// resource.
///
/// `Undecodable` is distinct from `Duplicate` because CSS Fonts says a source
/// that fails to *download or parse* falls through to the next `src:` entry,
/// while a source that merely repeats one already registered has succeeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebFontOutcome {
    /// The blob decoded and a new face joined the collection: callers bump the
    /// fonts version and re-shape.
    Registered,
    /// These exact bytes and descriptors are already registered for this family.
    Duplicate,
    /// The blob is not a font we can decode (or carried no usable face).
    Undecodable,
}

impl WebFontOutcome {
    /// True when the family now resolves to this font — i.e. no `src:` fallback
    /// is warranted.
    #[must_use]
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Registered | Self::Duplicate)
    }
}

/// The fontique matching attributes for a web font, derived from an
/// `@font-face` rule's descriptors (Phase 7, WP-C). The face is bound to the
/// CSS family name via [`FontSystem::register_web_font`] regardless of the
/// font's own internal name table.
#[derive(Clone, Copy, Debug)]
pub struct WebFontAttrs {
    width: FontWidth,
    style: FontStyle,
    /// The registered weight, or `None` when the `@font-face` declares a weight
    /// *range* (a variable font). Overriding a range to a single value would
    /// mismatch heavier/lighter requests, so we leave the weight unset and let
    /// fontique match against the font's own `fvar` weight axis.
    weight: Option<FontWeight>,
}

impl WebFontAttrs {
    /// Maps a resolved [`FontFaceInfo`]'s width/style/weight descriptors to
    /// fontique attributes. A single-value `font-weight` descriptor overrides
    /// the face's weight; a range (`font-weight: 100 900`, a variable font)
    /// leaves it unset so the whole axis remains matchable.
    #[must_use]
    pub fn from_face(info: &FontFaceInfo) -> Self {
        let (min, max) = info.weight;
        Self {
            width: FontWidth::from_percentage(info.stretch),
            weight: (min == max).then(|| FontWeight::new(min)),
            style: match info.style {
                FontFaceStyle::Normal => FontStyle::Normal,
                FontFaceStyle::Italic => FontStyle::Italic,
                FontFaceStyle::Oblique(degrees) => FontStyle::Oblique(Some(degrees)),
            },
        }
    }
}

/// The WPT Ahem font (public domain; see `assets/PROVENANCE.md`). Registered
/// unconditionally so metric-dependent tests are deterministic across
/// platforms.
pub const AHEM_FONT: &[u8] = include_bytes!("../assets/Ahem.ttf");

/// Set by [`disable_system_fonts`]; read when a [`FontSystem`] is built.
static SYSTEM_FONTS_DISABLED: AtomicBool = AtomicBool::new(false);

/// Turns off system-font discovery for the rest of the process, exactly as a
/// build without the `system_fonts` feature behaves: the bundled Ahem font backs
/// every generic family, so text metrics are identical on every platform.
///
/// A compile-time feature cannot do this job. Cargo unifies features across a
/// build, so any workspace member that enables `layout/system_fonts` — the
/// default — turns it on for `cargo test --workspace`, `cargo xtask golden` and
/// `cargo xtask reftest` alike, no matter what those crates declare. The
/// deterministic runners therefore call this before rendering anything.
///
/// Only affects [`FontSystem`]s built afterwards; call it before any page is
/// created.
pub fn disable_system_fonts() {
    SYSTEM_FONTS_DISABLED.store(true, AtomicOrdering::Relaxed);
}

/// Whether system fonts should be discovered: compiled in, and not switched off.
fn system_fonts_enabled() -> bool {
    cfg!(feature = "system_fonts") && !SYSTEM_FONTS_DISABLED.load(AtomicOrdering::Relaxed)
}

/// Enumerates the system fonts (when enabled) and registers the bundled Ahem
/// font. This is the expensive part of building a [`FontSystem`] — 60–90 ms with
/// system fonts — which is why it runs once per process, behind
/// [`font_context_template`].
fn build_font_context(system_fonts: bool) -> FontContext {
    let mut font_ctx = FontContext {
        source_cache: SourceCache::new_shared(),
        collection: Collection::new(CollectionOptions {
            shared: false,
            system_fonts,
        }),
    };

    let registered = font_ctx
        .collection
        .register_fonts(Blob::new(Arc::new(AHEM_FONT) as _), None);
    if !system_fonts {
        let family_ids: Vec<_> = registered.iter().map(|(id, _)| *id).collect();
        for generic in [
            GenericFamily::SansSerif,
            GenericFamily::Serif,
            GenericFamily::Monospace,
            GenericFamily::Cursive,
            GenericFamily::Fantasy,
            GenericFamily::SystemUi,
        ] {
            font_ctx
                .collection
                .append_generic_families(generic, family_ids.iter().copied());
        }
    }

    font_ctx
}

/// A clone of the process-wide warm font context, built on first use.
///
/// Cloning a fontique `Collection` bumps refcounts on the shared `System`
/// (`Arc<Mutex<SystemFonts>>` plus the family-name and generic-family maps) —
/// the system-font enumeration is *not* repeated — and deep-copies only the
/// collection's own data, which at this point holds a single family (Ahem) and
/// the generic-family layout. So the clone costs microseconds while the scan
/// happens once.
///
/// Web fonts stay isolated: the collection is built with
/// `CollectionOptions { shared: false }`, so `register_fonts` writes into the
/// clone's private data and never reaches the template or a sibling page. That
/// isolation is a property of fontique, not a convention we uphold — see
/// `web_fonts_do_not_leak_between_font_systems`.
///
/// The two slots key on [`system_fonts_enabled`], which preserves the
/// [`disable_system_fonts`] contract verbatim: flipping the latch after a page
/// has already warmed the system-fonts slot still yields an Ahem-only context.
///
/// **Consequence:** the set of system fonts is frozen for the lifetime of the
/// process — a font installed in the OS while the engine runs is not picked up
/// by a later page. For a headless engine that is a determinism win; should a
/// long-lived embedder ever need otherwise, the escape hatch is a
/// `refresh_system_fonts()` that clears both slots.
fn font_context_template(system_fonts: bool) -> FontContext {
    static TEMPLATES: Mutex<[Option<FontContext>; 2]> = Mutex::new([None, None]);

    // Recover a poisoned lock rather than propagating: a panic inside one font
    // scan must not permanently kill every subsequent `Page::new`.
    // `get_or_insert_with` leaves the slot `None` when the build unwinds, so the
    // next caller simply retries it.
    let mut slots = TEMPLATES.lock().unwrap_or_else(PoisonError::into_inner);
    slots[usize::from(system_fonts)]
        .get_or_insert_with(|| build_font_context(system_fonts))
        .clone()
}

/// Shared font collection + parley layout context.
pub struct FontSystem {
    font_ctx: Arc<Mutex<FontContext>>,
    layout_ctx: LayoutContext<TextBrush>,
    /// `(family, blob-hash)` of every web font already registered, so a repeated
    /// `@font-face` load is not registered twice into the collection.
    web_fonts: HashSet<(String, u64)>,
}

impl FontSystem {
    /// Builds the font system: system-font discovery per the `system_fonts`
    /// feature (unless [`disable_system_fonts`] switched it off), with the
    /// bundled Ahem font always registered. Without system fonts, Ahem also backs
    /// every generic family so text always shapes.
    ///
    /// The collection is cloned from a process-wide template
    /// ([`font_context_template`]), so the system-font scan runs once per
    /// process rather than once per page. The clone is private: `@font-face`
    /// faces registered here are invisible to every other `FontSystem`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            font_ctx: Arc::new(Mutex::new(font_context_template(system_fonts_enabled()))),
            // Per-page shaping scratch and per-page `@font-face` dedup: both
            // start empty, neither is shared with the template.
            layout_ctx: LayoutContext::new(),
            web_fonts: HashSet::new(),
        }
    }

    /// The shared font context handle (cloned into the style engine's
    /// font-metrics provider).
    #[must_use]
    pub fn font_ctx(&self) -> Arc<Mutex<FontContext>> {
        Arc::clone(&self.font_ctx)
    }

    /// Decodes a downloaded `@font-face` resource (WOFF2/WOFF → sfnt, or raw
    /// sfnt pass-through) and registers it into the shared collection under the
    /// CSS `family` name with the given matching attributes (Phase 7, WP-C).
    ///
    /// Because the collection is shared with the style engine's metrics provider
    /// (same `Arc<Mutex<FontContext>>`), a newly registered face is immediately
    /// visible to shaping and metrics queries.
    pub fn register_web_font(
        &mut self,
        family: &str,
        raw: &[u8],
        attrs: WebFontAttrs,
    ) -> WebFontOutcome {
        let Some(sfnt) = crate::webfont::decode_font(raw) else {
            return WebFontOutcome::Undecodable;
        };

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        sfnt.hash(&mut hasher);
        // Fold the matching descriptors into the dedup key: two `@font-face`
        // rules with the same family + bytes but a different weight/style/width
        // (e.g. a regular and a bold sharing one variable-font file) are
        // distinct faces, so keying on family+bytes alone would silently drop
        // the second and lose its override.
        attrs.width.ratio().to_bits().hash(&mut hasher);
        attrs.weight.map(|w| w.value().to_bits()).hash(&mut hasher);
        match attrs.style {
            FontStyle::Normal => 0u8.hash(&mut hasher),
            FontStyle::Italic => 1u8.hash(&mut hasher),
            FontStyle::Oblique(angle) => {
                2u8.hash(&mut hasher);
                angle.map(f32::to_bits).hash(&mut hasher);
            }
        }
        let key = (family.to_owned(), hasher.finish());
        if !self.web_fonts.insert(key) {
            // Identical bytes + descriptors for this family already registered.
            return WebFontOutcome::Duplicate;
        }

        let blob = Blob::new(Arc::new(sfnt));
        let mut font_ctx = self.font_ctx.lock().expect("font context poisoned");
        let registered = font_ctx.collection.register_fonts(
            blob,
            Some(FontInfoOverride {
                family_name: Some(family),
                width: Some(attrs.width),
                style: Some(attrs.style),
                weight: attrs.weight,
                axes: None,
            }),
        );
        if registered.is_empty() {
            WebFontOutcome::Undecodable
        } else {
            WebFontOutcome::Registered
        }
    }

    /// Runs `f` with both shaping contexts locked/borrowed.
    pub fn with_contexts<R>(
        &mut self,
        f: impl FnOnce(&mut FontContext, &mut LayoutContext<TextBrush>) -> R,
    ) -> R {
        let mut font_ctx = self.font_ctx.lock().expect("font context poisoned");
        f(&mut font_ctx, &mut self.layout_ctx)
    }
}

impl Default for FontSystem {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ahem_is_registered() {
        let fonts = FontSystem::new();
        let ctx = fonts.font_ctx();
        let mut ctx = ctx.lock().unwrap();
        assert!(ctx.collection.family_by_name("Ahem").is_some());
    }

    const TEST_WOFF2: &[u8] = include_bytes!("../assets/webfont/test.woff2");

    fn normal_attrs() -> WebFontAttrs {
        WebFontAttrs::from_face(&FontFaceInfo {
            family: "WebTest".into(),
            sources: Vec::new(),
            unicode_range: None,
            weight: (400.0, 400.0),
            style: FontFaceStyle::Normal,
            stretch: 100.0,
        })
    }

    #[test]
    fn register_web_font_binds_family() {
        let mut fonts = FontSystem::new();
        // A web font is unknown until registered, then resolves under its CSS
        // family name regardless of the font's own internal name table.
        assert_eq!(
            fonts.register_web_font("WebTest", TEST_WOFF2, normal_attrs()),
            WebFontOutcome::Registered
        );
        // Re-registering identical bytes for the same family is a no-op.
        assert_eq!(
            fonts.register_web_font("WebTest", TEST_WOFF2, normal_attrs()),
            WebFontOutcome::Duplicate
        );

        let ctx = fonts.font_ctx();
        let mut ctx = ctx.lock().unwrap();
        assert!(ctx.collection.family_by_name("WebTest").is_some());
    }

    #[test]
    fn bad_font_bytes_do_not_register() {
        let mut fonts = FontSystem::new();
        // `Undecodable` (not `Duplicate`) is what makes the page fall through to
        // the next `src:` entry.
        assert_eq!(
            fonts.register_web_font("Nope", b"definitely not a font", normal_attrs()),
            WebFontOutcome::Undecodable
        );
        let ctx = fonts.font_ctx();
        let mut ctx = ctx.lock().unwrap();
        assert!(ctx.collection.family_by_name("Nope").is_none());
    }

    fn attrs_with_weight(weight: (f32, f32)) -> WebFontAttrs {
        WebFontAttrs::from_face(&FontFaceInfo {
            family: "F".into(),
            sources: Vec::new(),
            unicode_range: None,
            weight,
            style: FontFaceStyle::Normal,
            stretch: 100.0,
        })
    }

    #[test]
    fn single_weight_is_overridden_but_a_range_is_left_to_the_fvar_axis() {
        // A single-value `font-weight` descriptor overrides the face's weight.
        assert!(
            attrs_with_weight((700.0, 700.0)).weight.is_some(),
            "single-value weight overrides"
        );
        // A range (`font-weight: 100 900`, a variable font) leaves the weight
        // unset so the whole axis stays matchable, instead of pinning to 100.
        assert!(
            attrs_with_weight((100.0, 900.0)).weight.is_none(),
            "range weight is left to the fvar axis"
        );
    }

    #[test]
    fn distinct_weights_sharing_bytes_both_register() {
        let mut fonts = FontSystem::new();
        // Regular and bold `@font-face` rules can legitimately share the same
        // family and the same font file, differing only in `font-weight`.
        assert_eq!(
            fonts.register_web_font("Shared", TEST_WOFF2, attrs_with_weight((400.0, 400.0))),
            WebFontOutcome::Registered,
            "first (regular) face registers"
        );
        // Regression: the dedup key used to be (family, hash(bytes)) only, so
        // this bold face collided with the regular one and never registered —
        // `font-weight: bold` silently had no effect.
        assert_eq!(
            fonts.register_web_font("Shared", TEST_WOFF2, attrs_with_weight((700.0, 700.0))),
            WebFontOutcome::Registered,
            "distinct-weight face sharing the same bytes must also register"
        );
        // An exact repeat (same bytes + same descriptors) is still a no-op.
        assert_eq!(
            fonts.register_web_font("Shared", TEST_WOFF2, attrs_with_weight((700.0, 700.0))),
            WebFontOutcome::Duplicate,
            "an identical re-registration remains deduplicated"
        );

        let ctx = fonts.font_ctx();
        let mut ctx = ctx.lock().unwrap();
        let family = ctx
            .collection
            .family_by_name("Shared")
            .expect("Shared family present");
        // Both faces resolved into the family (regular + bold), not just one.
        assert!(
            family.fonts().len() >= 2,
            "both weights resolve into the family: {family:?}"
        );
    }

    fn family_is_known(fonts: &FontSystem, name: &str) -> bool {
        let ctx = fonts.font_ctx();
        let mut ctx = ctx.lock().unwrap();
        ctx.collection.family_by_name(name).is_some()
    }

    #[test]
    fn web_fonts_do_not_leak_between_font_systems() {
        // Every collection is a clone of one process-wide template, which makes
        // isolation the load-bearing invariant: a page's `@font-face` must reach
        // neither a sibling page nor the template (and through it, every page
        // built later).
        let sibling = FontSystem::new();
        let mut owner = FontSystem::new();
        assert_eq!(
            owner.register_web_font("Isolated", TEST_WOFF2, normal_attrs()),
            WebFontOutcome::Registered
        );
        let later = FontSystem::new();

        assert!(
            family_is_known(&owner, "Isolated"),
            "the registering font system sees its own web font"
        );
        assert!(
            !family_is_known(&sibling, "Isolated"),
            "a font system built earlier must not see another page's web font"
        );
        assert!(
            !family_is_known(&later, "Isolated"),
            "nor must one built afterwards — i.e. the shared template stayed clean"
        );
    }

    #[test]
    fn the_font_context_template_is_reused() {
        // `FamilyId`s are handed out by a process-wide atomic counter, so two
        // independently built collections physically cannot give Ahem the same
        // id. Equal ids therefore prove both contexts were cloned from one
        // template — pinning the cache without reaching into its internals, and
        // without a wall-clock threshold that would flake on a loaded CI box.
        let ahem_id = |fonts: &FontSystem| {
            let ctx = fonts.font_ctx();
            let mut ctx = ctx.lock().unwrap();
            ctx.collection
                .family_by_name("Ahem")
                .expect("Ahem is always registered")
                .id()
        };
        assert_eq!(
            ahem_id(&FontSystem::new()),
            ahem_id(&FontSystem::new()),
            "both font systems clone the same warm template"
        );
    }
}

/// A [`FontMetricsProvider`] backed by the shared parley font collection
/// (adapted from blitz-dom `font_metrics.rs`): resolves `ex`/`ch`/`ic` units
/// and `font-size-adjust` via skrifa metrics of the matched font.
#[derive(Clone)]
pub struct ParleyFontMetricsProvider {
    pub font_ctx: Arc<Mutex<FontContext>>,
}

impl core::fmt::Debug for ParleyFontMetricsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ParleyFontMetricsProvider")
    }
}

impl style::device::servo::FontMetricsProvider for ParleyFontMetricsProvider {
    fn query_font_metrics(
        &self,
        _vertical: bool,
        font_styles: &style::properties::style_structs::Font,
        font_size: style::values::computed::CSSPixelLength,
        _flags: style::values::computed::font::QueryFontMetricsFlags,
    ) -> style::font_metrics::FontMetrics {
        use parley::FontVariation;
        use parley::fontique::{Attributes, Query, QueryFont, QueryStatus};
        use skrifa::MetadataProvider as _;
        use skrifa::charmap::Charmap;
        use skrifa::instance::{LocationRef, Size};
        use skrifa::metrics::{GlyphMetrics, Metrics};
        use style::values::computed::CSSPixelLength;

        use crate::text;

        // Lock font_ctx. Explicit reborrow required for the borrow checker.
        let mut font_ctx = self.font_ctx.lock().expect("font context poisoned");
        let font_ctx = &mut *font_ctx;

        // Query fontique for the font that matches the font styles.
        let mut query = font_ctx.collection.query(&mut font_ctx.source_cache);
        let families = font_styles
            .font_family
            .families
            .iter()
            .map(text::query_font_family);
        query.set_families(families);
        query.set_attributes(Attributes {
            width: text::font_width(font_styles.font_stretch),
            weight: text::font_weight(font_styles.font_weight),
            style: text::font_style(font_styles.font_style),
        });

        let variations = text::font_variations(&font_styles.font_variation_settings);

        fn find_font_for(query: &mut Query<'_>, ch: char) -> Option<QueryFont> {
            let mut font = None;
            query.matches_with(|q_font: &QueryFont| {
                let Ok(font_ref) = skrifa::FontRef::from_index(q_font.blob.as_ref(), q_font.index)
                else {
                    return QueryStatus::Continue;
                };

                let charmap = font_ref.charmap();
                if charmap.map(ch).is_some() {
                    font = Some(q_font.clone());
                    QueryStatus::Stop
                } else {
                    QueryStatus::Continue
                }
            });
            font
        }

        fn advance_of(
            query: &mut Query<'_>,
            ch: char,
            font_size: Size,
            variations: &[FontVariation],
        ) -> Option<f32> {
            let font = find_font_for(query, ch)?;
            let font_ref = skrifa::FontRef::from_index(font.blob.as_ref(), font.index).ok()?;
            let location = font_ref.axes().location(
                variations
                    .iter()
                    .map(|v| (skrifa::Tag::from_be_bytes(v.tag.to_bytes()), v.value)),
            );
            let location_ref = LocationRef::from(&location);
            let glyph_metrics = GlyphMetrics::new(&font_ref, font_size, location_ref);
            let char_map = Charmap::new(&font_ref);
            let glyph_id = char_map.map(ch)?;
            glyph_metrics.advance_width(glyph_id)
        }

        fn metrics_of(
            query: &mut Query<'_>,
            ch: char,
            font_size: Size,
            variations: &[FontVariation],
        ) -> Option<(f32, Option<f32>, Option<f32>)> {
            let font = find_font_for(query, ch)?;
            let font_ref = skrifa::FontRef::from_index(font.blob.as_ref(), font.index).ok()?;
            let location = font_ref.axes().location(
                variations
                    .iter()
                    .map(|v| (skrifa::Tag::from_be_bytes(v.tag.to_bytes()), v.value)),
            );
            let location_ref = LocationRef::from(&location);
            let metrics = Metrics::new(&font_ref, font_size, location_ref);
            Some((metrics.ascent, metrics.x_height, metrics.cap_height))
        }

        let font_size = Size::new(font_size.px());
        let zero_advance = advance_of(&mut query, '0', font_size, &variations);
        let ic_advance = advance_of(&mut query, '\u{6C34}', font_size, &variations);
        let (ascent, x_height, cap_height) =
            metrics_of(&mut query, ' ', font_size, &variations).unwrap_or((0.0, None, None));

        style::font_metrics::FontMetrics {
            ascent: CSSPixelLength::new(ascent),
            x_height: x_height.filter(|xh| *xh != 0.0).map(CSSPixelLength::new),
            cap_height: cap_height.map(CSSPixelLength::new),
            zero_advance_measure: zero_advance.map(CSSPixelLength::new),
            ic_width: ic_advance.map(CSSPixelLength::new),
            script_percent_scale_down: None,
            script_script_percent_scale_down: None,
        }
    }

    fn base_size_for_generic(
        &self,
        generic: style::values::computed::font::GenericFontFamily,
    ) -> style::values::computed::Length {
        let size = match generic {
            style::values::computed::font::GenericFontFamily::Monospace => 13.0,
            _ => 16.0,
        };
        style::values::computed::Length::from(app_units::Au::from_f32_px(size))
    }
}
