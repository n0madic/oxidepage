//! The image store: decoded images keyed by absolute URL, shared between
//! layout (intrinsic sizing of replaced elements) and paint (rasterization).
//!
//! Decoding and network loading happen in the page (WP-K); layout only reads
//! intrinsic sizes here. A monotonic version counter feeds the reflow/paint
//! stamps so a newly-decoded image triggers relayout and repaint.
//!
//! An image is stored either as decoded pixels or, for SVG, as the source
//! markup: a vector image is rasterized by the backend at the size it actually
//! paints at, so an icon shown far above its `viewBox` stays sharp (ADR-0013
//! D5). Either way `width`/`height` are the intrinsic size in CSS px — the only
//! thing layout reads.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use oxidepage_base::NodeId;
use oxidepage_dom::DomTree;
use oxidepage_dom::node::ElementData;
use oxidepage_dom::serialize::outer_html;
use style::color::ColorSpace;
use style::properties::ComputedValues;

/// The store key for an inline `<svg>` element, derived from its serialized
/// markup, its computed `color`, and the custom properties its `var()`s resolve
/// to: the page rasterizes the markup under this key and box construction looks
/// the entry up under the same one, so identical icons (a sprite used dozens of
/// times) share one entry and a mutated `<svg>` gets its own.
///
/// `color` and `vars` are part of the key because they are part of the *source*:
/// [`inline_svg_source`] embeds the color so `fill="currentColor"` resolves and
/// substitutes the variables so `fill="var(--x)"` resolves, so two
/// otherwise-identical icons in different colors — or the same icon themed light
/// vs dark — are different images.
#[must_use]
pub fn inline_svg_key(markup: &str, color: [u8; 4], vars: &[(String, String)]) -> String {
    // FNV-1a: no dependency, and a collision would only mean two distinct SVGs
    // sharing a rasterization, not unsafety.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    };
    for byte in markup.as_bytes() {
        mix(*byte);
    }
    for byte in color {
        mix(byte);
    }
    // The resolved custom properties are part of the *source* too (see
    // [`inline_svg_source`]): the same markup renders differently in light vs
    // dark mode when `fill="var(--x)"` resolves to different values, so a
    // change to any referenced variable must produce a new key. `\0` between
    // fields keeps ("--a","bc") distinct from ("--ab","c").
    for (name, value) in vars {
        for byte in name.as_bytes() {
            mix(*byte);
        }
        mix(0);
        for byte in value.as_bytes() {
            mix(*byte);
        }
        mix(0);
    }
    format!("inline-svg:{hash:016x}")
}

/// The markup an inline `<svg>` is rasterized from, with any sprite `<use>`
/// references resolved so resvg can render them.
///
/// resvg renders each `<svg>` in isolation and resolves `<use href="#id">` only
/// against definitions in the *same* tree. An icon sprite keeps its `<symbol>`
/// definitions in one hidden `<svg>` and points many little
/// `<svg><use href="#icon"></svg>` elements at them from elsewhere in the
/// document, so the fragment resvg sees for one icon has nothing to resolve and
/// decodes to a broken-image placeholder (a grey square). This inlines every
/// referenced definition into the fragment. Two further fix-ups the isolated
/// fragment needs, both because the sprite leaned on the surrounding document:
///
/// * `xmlns:xlink` is declared when the markup uses the `xlink:` prefix — usvg
///   parses as strict XML and rejects the *whole* document on an undeclared
///   prefix, and the HTML serializer emits `xlink:href` without the sprite
///   root's namespace declaration.
/// * a root `viewBox` is synthesized from the referenced symbol when the `<svg>`
///   carries no `width`/`height`/`viewBox` of its own (sprite icons size purely
///   from CSS), without which usvg reports `InvalidSize` and rejects it.
///
/// Both box construction (which looks the store entry up) and the page (which
/// rasterizes it) route through here, so the keys they derive from the markup
/// agree. Self-contained SVGs — the overwhelming majority — return unchanged.
#[must_use]
pub fn inline_svg_markup(dom: &DomTree, svg: NodeId) -> String {
    let markup = outer_html(dom, svg);
    if !markup.contains("<use") {
        return markup;
    }

    // One pass over the subtree: the fragment ids its `<use>`s reference, and the
    // ids it already defines (a self-contained `<use>` needs no inlining).
    let mut wanted: Vec<String> = Vec::new();
    let mut local_ids: HashSet<String> = HashSet::new();
    for node in dom.inclusive_descendants(svg) {
        let Some(el) = dom.node(node).as_element() else {
            continue;
        };
        if let Some(id) = svg_attr(el, "id") {
            local_ids.insert(id.to_owned());
        }
        if &*el.name.local == "use"
            && let Some(id) = use_fragment_target(el)
            && !wanted.iter().any(|w| w == id)
        {
            wanted.push(id.to_owned());
        }
    }

    // Serialize each externally-defined referent, capturing a `viewBox` to fall
    // back on for the root's size.
    let mut defs = String::new();
    let mut symbol_viewbox: Option<String> = None;
    for id in &wanted {
        if local_ids.contains(id) {
            continue;
        }
        // Scoped to the referring `<svg>`'s own document: a `<use href="#x">`
        // must not reach into another browsing context for its definition.
        let Some(def) = dom
            .containing_document(svg)
            .and_then(|doc| dom.element_by_id(doc, id))
        else {
            continue;
        };
        // Never inline the icon itself or one of its ancestors: that would
        // duplicate the definition or splice a whole subtree into itself.
        if dom.inclusive_ancestors(svg).any(|a| a == def) {
            continue;
        }
        defs.push_str(&outer_html(dom, def));
        if symbol_viewbox.is_none() {
            symbol_viewbox = dom
                .node(def)
                .as_element()
                .and_then(|el| svg_attr(el, "viewBox"))
                .map(str::to_owned);
        }
    }

    let Some(tag_end) = start_tag_end(&markup) else {
        return markup;
    };
    let start_tag = &markup[..tag_end];

    let needs_xlink = (markup.contains("xlink:") || defs.contains("xlink:"))
        && !has_attr(start_tag, "xmlns:xlink");
    let needs_viewbox =
        !defs.is_empty() && symbol_viewbox.is_some() && !root_has_intrinsic_size(start_tag);

    if defs.is_empty() && !needs_xlink && !needs_viewbox {
        return markup;
    }

    // Insert the new attributes just before the start tag's `>` (or `/>`) and the
    // inlined definitions just after it, ahead of the original children.
    let mut attr_at = tag_end - 1;
    if markup.as_bytes()[..attr_at].last() == Some(&b'/') {
        attr_at -= 1;
    }
    let mut out = String::with_capacity(markup.len() + defs.len() + 64);
    out.push_str(&markup[..attr_at]);
    if needs_xlink {
        out.push_str(" xmlns:xlink=\"http://www.w3.org/1999/xlink\"");
    }
    if needs_viewbox && let Some(vb) = &symbol_viewbox {
        out.push_str(" viewBox=\"");
        out.push_str(vb);
        out.push('"');
    }
    out.push_str(&markup[attr_at..tag_end]);
    out.push_str(&defs);
    out.push_str(&markup[tag_end..]);
    out
}

/// Value of the attribute named `local` on an SVG element, matched by local name
/// (ignoring any namespace prefix the way the SVG content model does — an
/// `xlink:href` and a bare `href` both have local name `href`).
fn svg_attr<'a>(el: &'a ElementData, local: &str) -> Option<&'a str> {
    el.attrs()
        .iter()
        .find(|a| &*a.name.local == local)
        .map(|a| &*a.value)
}

/// The local fragment id a `<use>` element targets via `href`/`xlink:href="#id"`.
/// External references (`sprite.svg#id`, a full URL) are not resolved.
fn use_fragment_target(el: &ElementData) -> Option<&str> {
    svg_attr(el, "href")
        .and_then(|v| v.strip_prefix('#'))
        .filter(|id| !id.is_empty())
}

/// Whether an `<svg>` start tag declares a size usvg can size the viewport from:
/// a `viewBox`, or both `width` and `height`.
fn root_has_intrinsic_size(start_tag: &str) -> bool {
    has_attr(start_tag, "viewBox")
        || (has_attr(start_tag, "width") && has_attr(start_tag, "height"))
}

/// Whether the start tag carries an attribute named `name` (ASCII
/// case-insensitively), scanning attribute names outside quoted values so a
/// match inside a value (`data-x="width=1"`) does not count.
fn has_attr(start_tag: &str, name: &str) -> bool {
    let b = start_tag.as_bytes();
    let mut i = 0;
    // `<` and the tag name.
    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' && b[i] != b'/' {
        i += 1;
    }
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() || b[i] == b'>' {
            return false;
        }
        let name_start = i;
        while i < b.len()
            && !b[i].is_ascii_whitespace()
            && b[i] != b'='
            && b[i] != b'>'
            && b[i] != b'/'
        {
            i += 1;
        }
        if start_tag[name_start..i].eq_ignore_ascii_case(name) {
            return true;
        }
        // Skip `=` and the value (quoted or not) before the next attribute.
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let quote = b[i];
                i += 1;
                while i < b.len() && b[i] != quote {
                    i += 1;
                }
                i = (i + 1).min(b.len());
            } else {
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
            }
        }
    }
    false
}

/// The markup to rasterize for an inline `<svg>`: the element's `outer_html`
/// with the element's computed `color` embedded, so `currentColor` inside the
/// SVG resolves to the CSS color it inherited. The SVG is rendered in isolation
/// by resvg, which knows nothing of the surrounding cascade, so without this
/// `fill="currentColor"` would fall back to black.
///
/// The declaration is prepended to the root `<svg>`'s `style` attribute (which
/// is created if absent). Prepending, not appending, is what makes an author's
/// own `color:` in that attribute still win — later declarations override
/// earlier ones.
#[must_use]
pub fn inline_svg_source(markup: &str, color: [u8; 4], vars: &[(String, String)]) -> String {
    // resvg (0.47) does not resolve `var()`, so an inline SVG that fills with a
    // CSS custom property — `fill="var(--site-background)"`, the way Tailwind and
    // friends theme icons — would fall back to the initial `fill` (black),
    // painting a solid blob. The cascade *did* resolve those variables on the
    // host `<svg>`, so substitute them into the markup before handing it over.
    let substituted = substitute_vars(markup, vars);
    let markup = substituted.as_str();

    let [r, g, b, _] = color;
    let declaration = format!("color:#{r:02x}{g:02x}{b:02x};");

    let Some(tag_end) = start_tag_end(markup) else {
        return markup.to_owned();
    };
    let mut out = String::with_capacity(markup.len() + declaration.len() + 9);
    match style_value_start(&markup[..tag_end]) {
        Some(at) => {
            out.push_str(&markup[..at]);
            out.push_str(&declaration);
            out.push_str(&markup[at..]);
        }
        None => {
            // No `style` attribute: add one just inside the start tag's `>`
            // (or `/>`).
            let mut at = tag_end - 1;
            if markup.as_bytes()[..at].last() == Some(&b'/') {
                at -= 1;
            }
            out.push_str(&markup[..at]);
            out.push_str(" style=\"");
            out.push_str(&declaration);
            out.push('"');
            out.push_str(&markup[at..]);
        }
    }
    out
}

/// The element's computed `color` as 8-bit sRGBA — what `currentColor` inside
/// an inline `<svg>` resolves to.
#[must_use]
pub fn current_color(style: &ComputedValues) -> [u8; 4] {
    let color = style.get_inherited_text().clone_color();
    let srgb = if matches!(color.color_space, ColorSpace::Srgb) {
        color
    } else {
        color.to_color_space(ColorSpace::Srgb)
    };
    let ch = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [
        ch(srgb.components.0),
        ch(srgb.components.1),
        ch(srgb.components.2),
        ch(srgb.alpha),
    ]
}

/// The `(--name, value)` pairs an inline `<svg>` needs to render: every custom
/// property its markup references through `var()`, resolved against the
/// element's computed style. Empty when the markup uses no `var()`.
///
/// resvg resolves no `var()` of its own, so [`inline_svg_source`] substitutes
/// these into the markup and [`inline_svg_key`] folds them into the store key —
/// the same icon themed light vs dark is two rasterizations. Both the page (when
/// it rasterizes) and box construction (when it looks the result up) compute
/// this from the same style so their keys agree.
#[must_use]
pub fn svg_var_substitutions(markup: &str, style: &ComputedValues) -> Vec<(String, String)> {
    // Cheap bail-out: the overwhelming majority of icons use no variables, and
    // this spares them the whole custom-property scan.
    if !markup.contains("var(") {
        return Vec::new();
    }
    let names = referenced_var_names(markup);
    if names.is_empty() {
        return Vec::new();
    }

    let custom = style.custom_properties();
    names
        .into_iter()
        .filter_map(|name| {
            // Computed custom-property names are stored without the `--` prefix.
            let bare = style::Atom::from(&name["--".len()..]);
            let mut i = 0;
            while let Some((prop_name, value)) = custom.property_at(i) {
                i += 1;
                if *prop_name != bare {
                    continue;
                }
                // `None` marks a property whose cascade removed it.
                let value = value.as_ref()?;
                return Some((name, value.to_variable_value().css));
            }
            None
        })
        .collect()
}

/// The distinct custom-property names (each including its `--` prefix, in first
/// appearance order) referenced by a `var()` anywhere in `markup`.
fn referenced_var_names(markup: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut rest = markup;
    while let Some(pos) = rest.find("var(") {
        let after = &rest[pos + "var(".len()..];
        let arg = after.trim_start();
        if let Some(name) = arg.strip_prefix("--") {
            let end = name
                .find(|c: char| c == ',' || c == ')' || c.is_whitespace())
                .unwrap_or(name.len());
            if end > 0 {
                let full = format!("--{}", &name[..end]);
                if !names.contains(&full) {
                    names.push(full);
                }
            }
        }
        rest = after;
    }
    names
}

/// Replaces every `var(--name)` / `var(--name, fallback)` in `input` with the
/// resolved value from `vars`, or — when the property did not resolve — the
/// fallback (itself substituted). An unresolved `var()` with no fallback is left
/// verbatim, matching the browser outcome (an invalid `fill` paints as the
/// initial black), so this stays a no-op for markup that references nothing.
fn substitute_vars(input: &str, vars: &[(String, String)]) -> String {
    if !input.contains("var(") {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find("var(") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + "var(".len()..];
        let Some(close) = matching_paren(after) else {
            // Unbalanced: copy the literal `var(` and carry on past it.
            out.push_str("var(");
            rest = after;
            continue;
        };
        let inner = &after[..close];
        let (name, fallback) = split_first_top_level_comma(inner);
        let name = name.trim();
        if let Some((_, value)) = vars.iter().find(|(n, _)| n == name) {
            out.push_str(value);
        } else if let Some(fallback) = fallback {
            out.push_str(&substitute_vars(fallback.trim(), vars));
        } else {
            out.push_str(&rest[pos..pos + "var(".len() + close + 1]);
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Byte offset of the `)` that closes the `(` just before `s`, tracking nesting.
fn matching_paren(s: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits a `var()` argument list at its first top-level comma into the name and
/// the (optional) fallback. Commas nested in a fallback's own `var()`/function
/// do not split.
fn split_first_top_level_comma(s: &str) -> (&str, Option<&str>) {
    let mut depth = 0usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => return (&s[..i], Some(&s[i + 1..])),
            _ => {}
        }
    }
    (s, None)
}

/// The offset just past the start tag's `>`. An attribute value may itself
/// contain `>`, so the scan tracks quoting instead of taking the first one.
fn start_tag_end(markup: &str) -> Option<usize> {
    let bytes = markup.as_bytes();
    let mut quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Some(i + 1),
                _ => {}
            },
        }
    }
    None
}

/// The offset of the first byte of the start tag's `style` attribute *value*,
/// if it has one. `tag` is the whole start tag, `<` through `>`.
fn style_value_start(tag: &str) -> Option<usize> {
    let b = tag.as_bytes();
    let mut i = 0;
    // `<` and the tag name.
    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' && b[i] != b'/' {
        i += 1;
    }
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() || b[i] == b'>' {
            return None;
        }
        let name_start = i;
        while i < b.len()
            && !b[i].is_ascii_whitespace()
            && b[i] != b'='
            && b[i] != b'>'
            && b[i] != b'/'
        {
            i += 1;
        }
        let name = &tag[name_start..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            // A valueless attribute; `i` already sits on the next one.
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        let value_start = if b[i] == b'"' || b[i] == b'\'' {
            let quote = b[i];
            i += 1;
            let start = i;
            while i < b.len() && b[i] != quote {
                i += 1;
            }
            i = (i + 1).min(b.len());
            start
        } else {
            let start = i;
            while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                i += 1;
            }
            start
        };
        if name.eq_ignore_ascii_case("style") {
            return Some(value_start);
        }
    }
    None
}

/// A stable id for a decoded image within an [`ImageStore`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ImageId(pub u64);

/// The content of a stored image.
#[derive(Clone)]
pub enum ImageData {
    /// Decoded pixels: straight alpha RGBA8, row-major, `width * height * 4`
    /// bytes. `Arc`-shared so the display list can carry them to any raster
    /// thread.
    Raster { rgba: Arc<Vec<u8>> },
    /// A vector source: SVG markup, rasterized by the backend at the final
    /// device size.
    Vector { svg: Arc<Vec<u8>> },
}

/// A stored image: its intrinsic size in CSS px (what layout sizes the replaced
/// box from) plus its content, raster or vector.
#[derive(Clone)]
pub struct DecodedImage {
    pub id: ImageId,
    pub width: u32,
    pub height: u32,
    pub data: ImageData,
}

impl std::fmt::Debug for DecodedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedImage")
            .field("id", &self.id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field(
                "kind",
                match &self.data {
                    ImageData::Raster { .. } => &"raster",
                    ImageData::Vector { .. } => &"vector",
                },
            )
            .finish_non_exhaustive()
    }
}

/// Decoded (and known-broken) images keyed by absolute URL.
#[derive(Default)]
pub struct ImageStore {
    by_url: HashMap<String, Arc<DecodedImage>>,
    /// URLs whose load or decode failed (a placeholder is painted).
    broken: HashSet<String>,
    next_id: u64,
    version: u64,
}

impl ImageStore {
    /// The decoded image for `url`, if present.
    #[must_use]
    pub fn get(&self, url: &str) -> Option<Arc<DecodedImage>> {
        self.by_url.get(url).cloned()
    }

    /// Whether `url` is known to have failed to load/decode.
    #[must_use]
    pub fn is_broken(&self, url: &str) -> bool {
        self.broken.contains(url)
    }

    /// A counter bumped on every insertion, feeding the reflow/paint stamps.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Inserts (or replaces) a decoded raster image for `url`.
    pub fn insert_raster(
        &mut self,
        url: String,
        width: u32,
        height: u32,
        rgba: Arc<Vec<u8>>,
    ) -> Arc<DecodedImage> {
        self.insert(url, width, height, ImageData::Raster { rgba })
    }

    /// Inserts (or replaces) a vector image for `url`: `width`/`height` are its
    /// intrinsic size, `svg` the markup the backend rasterizes.
    pub fn insert_vector(
        &mut self,
        url: String,
        width: u32,
        height: u32,
        svg: Arc<Vec<u8>>,
    ) -> Arc<DecodedImage> {
        self.insert(url, width, height, ImageData::Vector { svg })
    }

    /// Inserts (or replaces) the image for `url`, assigning a fresh id and
    /// bumping the version. Returns the stored image.
    fn insert(
        &mut self,
        url: String,
        width: u32,
        height: u32,
        data: ImageData,
    ) -> Arc<DecodedImage> {
        self.next_id += 1;
        let image = Arc::new(DecodedImage {
            id: ImageId(self.next_id),
            width,
            height,
            data,
        });
        self.broken.remove(&url);
        self.by_url.insert(url, Arc::clone(&image));
        self.version += 1;
        image
    }

    /// Marks `url` as broken (failed load/decode) and bumps the version, so a
    /// placeholder is painted and layout stops treating it as pending.
    pub fn insert_broken(&mut self, url: String) {
        self.by_url.remove(&url);
        self.broken.insert(url);
        self.version += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{inline_svg_key, inline_svg_source, referenced_var_names, substitute_vars};

    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 128, 0, 255];

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn source_creates_a_style_attribute_when_absent() {
        let out = inline_svg_source(r#"<svg width="2"><rect/></svg>"#, GREEN, &[]);
        assert_eq!(
            out,
            r#"<svg width="2" style="color:#008000;"><rect/></svg>"#
        );
    }

    #[test]
    fn source_prepends_to_an_existing_style_attribute() {
        // Prepended, so the author's own declarations come after and win.
        let out = inline_svg_source(r#"<svg style="fill:red"><rect/></svg>"#, GREEN, &[]);
        assert_eq!(out, r#"<svg style="color:#008000;fill:red"><rect/></svg>"#);
    }

    #[test]
    fn source_ignores_quoted_gt_and_style_inside_attribute_values() {
        // The `>` and the `style=` here are *inside* an attribute value: the
        // start tag does not end at that `>`, and that is not a style attribute.
        let out = inline_svg_source(
            r#"<svg data-x="a>b style=c" width="2"><rect/></svg>"#,
            BLACK,
            &[],
        );
        assert_eq!(
            out,
            r#"<svg data-x="a>b style=c" width="2" style="color:#000000;"><rect/></svg>"#
        );
    }

    #[test]
    fn source_handles_a_self_closing_root() {
        let out = inline_svg_source(r#"<svg width="2"/>"#, BLACK, &[]);
        assert_eq!(out, r#"<svg width="2" style="color:#000000;"/>"#);
    }

    #[test]
    fn key_separates_colors() {
        let markup = r#"<svg><rect fill="currentColor"/></svg>"#;
        assert_ne!(
            inline_svg_key(markup, BLACK, &[]),
            inline_svg_key(markup, GREEN, &[]),
            "a color change must produce a new store key, or the icon never re-rasterizes"
        );
    }

    #[test]
    fn source_substitutes_a_referenced_variable() {
        // Regression: resvg does not resolve `var()`, so without this pass the
        // fill falls back to black. The resolved value is inlined into the
        // markup that reaches resvg.
        let out = inline_svg_source(
            r#"<svg><rect fill="var(--bg)"/></svg>"#,
            BLACK,
            &vars(&[("--bg", "#ffffff")]),
        );
        assert!(
            out.contains(r##"fill="#ffffff""##),
            "var() not substituted: {out}"
        );
        assert!(!out.contains("var("), "a var() survived: {out}");
    }

    #[test]
    fn substitute_uses_fallback_when_unresolved() {
        assert_eq!(
            substitute_vars("fill:var(--missing, #123456)", &[]),
            "fill:#123456"
        );
    }

    #[test]
    fn substitute_prefers_the_resolved_value_over_the_fallback() {
        assert_eq!(
            substitute_vars("var(--c, red)", &vars(&[("--c", "blue")])),
            "blue"
        );
    }

    #[test]
    fn substitute_leaves_an_unresolved_var_without_fallback_verbatim() {
        // Browsers treat this as an invalid value (black fill); keeping the
        // literal reproduces that rather than inventing a colour.
        assert_eq!(substitute_vars("var(--x)", &[]), "var(--x)");
    }

    #[test]
    fn substitute_resolves_a_nested_fallback() {
        assert_eq!(
            substitute_vars("var(--a, var(--b, green))", &vars(&[("--b", "orange")])),
            "orange"
        );
    }

    #[test]
    fn key_separates_resolved_variables() {
        let markup = r#"<svg><rect fill="var(--bg)"/></svg>"#;
        assert_ne!(
            inline_svg_key(markup, BLACK, &vars(&[("--bg", "#ffffff")])),
            inline_svg_key(markup, BLACK, &vars(&[("--bg", "#000000")])),
            "the same icon themed light vs dark must not share a rasterization"
        );
    }

    #[test]
    fn referenced_names_are_deduplicated_in_order() {
        let names = referenced_var_names(
            r#"<svg><a fill="var(--x)"/><b fill="var(--y, var(--x))"/></svg>"#,
        );
        assert_eq!(names, vec!["--x".to_owned(), "--y".to_owned()]);
    }
}
