# ADR-0008: Phase 7 web-font implementation decisions

- Status: accepted
- Date: 2026-07-06

## Context

Phase 6 rendered text only from the bundled **Ahem** font and OS system fonts:
`@font-face` rules parsed and cascaded but were never read, fetched, or
registered (ADR-0005), and PDF text was emitted as filled glyph **outlines**
with no font dictionary, so it was not selectable/extractable (ADR-0007 D3).

Phase 7 (design doc §5.10, §10) closes both gaps:

1. Discover `@font-face` rules, fetch their `src:` fonts through the net stack,
   decode **WOFF2/WOFF/TTF/OTF** to raw sfnt, and register the blob into the
   fontique collection under the CSS family name so shaping resolves it.
2. Make PDF text real: **subset + embed** the used fonts (`Type0` +
   `CIDFontType2` + `FontFile2`) with a `ToUnicode` CMap → extractable text.

The key enabler was already in place: the display list carries the raw font
`Blob` per glyph run (`FontResource`), so once a web font is registered and a
reshape is triggered, it flows to paint/raster/PDF unchanged. The injection
surface is therefore `style` (rule discovery), `page` (fetch), `layout`
(decode + registration + a reflow trigger), and `export-pdf` (subset/embed).

This ADR records the decisions (D1–D8), the deviations, and the v1 limits.

## Decision

### D1. WOFF/WOFF2 decode via the `wuff` crate

`crates/layout/src/webfont.rs::decode_font` sniffs the 4-byte signature and
routes: `wOF2` → `wuff::decompress_woff2` (brotli), `wOFF` →
`wuff::decompress_woff1` (zlib), and raw sfnt (`0x00010000` / `OTTO` / `true` /
`ttcf`) is passed through; anything else or a decode error returns `None`
(non-fatal). [`wuff`](https://docs.rs/wuff) is nicoburns' pure-Rust WOFF and
WOFF2 decoder — the Blitz author whose font patterns `layout/fonts.rs` already
mirrors. Its decompressors (`brotli-decompressor`, `flate2`) are pure-Rust and
already in the tree, so this adds no C/NASM toolchain (the same rationale as
ring-over-aws-lc-rs for Windows CI) and no hand-rolled ~500-line transform
decoder.

### D2. `@font-face` discovery walks author sheets' effective rules

`StyleEngine::font_faces` iterates each author stylesheet's
`StylesheetContents::effective_rules(device, custom_media, guard)`, which
descends into `@media`/`@supports`/`@layer`/`@import` honoring media evaluation,
and lifts each `CssRule::FontFace` into a backend-neutral `FontFaceInfo`
(family, ordered `src:` list, width/style/weight/unicode-range descriptors).

This deviates from the plan's original approach of reading stylo's aggregated
`Stylist::iter_extra_data_origins() → ExtraStyleData.font_faces`: that field is
public but is a `LayerOrderedVec` whose inner `Vec` is private and exposes **no
public iteration**, so it cannot be read from outside the crate. Walking the
effective rules is the supported path and still covers `@media`/`@import`. An
empty `CustomMediaMap` is passed (the experimental `@custom-media` feature is
not tracked globally).

### D3. Web fonts bind to the CSS family via `FontInfoOverride`

`FontSystem::register_web_font` decodes (D1) then calls
`Collection::register_fonts(Blob, Some(FontInfoOverride { family_name, width,
style, weight, axes: None }))`, which binds the blob to the CSS `font-family`
name regardless of the font's own `name` table. The `@font-face` descriptors
map to fontique attributes: stretch as a percentage, style as
normal/italic/oblique, and weight as a single value **only when the descriptor
is a single value**; a weight *range* (`font-weight: 100 900`, a variable font)
leaves the override unset so fontique matches the font's own `fvar` weight axis
instead of pinning to the range start. Registration is deduplicated by
`(family, blob-hash)`. Because the collection is
the same `Arc<Mutex<FontContext>>` shared with the style engine's metrics
provider, a newly registered face is immediately visible to shaping and metrics.

### D4. `fonts_version` forces a re-shape (mirrors `images_version`)

Registering a web font bumps a `LayoutEngine::fonts_version`, folded into both
`ReflowStamp` and `PaintStamp`; the incremental-relayout `try_patch` bails to a
full rebuild on a `fonts_version` change, exactly as it does for
`images_version`, so text that previously fell back re-shapes against the new
face. **font-display** is therefore effectively **block** in v1: rendering waits
for in-flight fonts (they join `in_flight`, so `settle`/`load`/screenshots/PDF
block on them).

### D5. The page font pipeline mirrors the image pipeline

`page` gains `PendingFont` / `pending_fonts` / `requested_fonts` /
`last_fontface_scan`, a `start_font_face_loads` scan (gated on
`dom.style_version()` like the background-image scan), a `handle_font_event`
(Headers/Chunk/Done/Error) routed through `dispatch_net_event`, and a
`finish_font` that calls `layout.register_web_font`. The scan is gated on the
**style engine's** `version()` (not `dom.style_version()`), so an external
`<link>`/`@import` sheet or a CSSOM `insertRule` that adds `@font-face` without a
DOM mutation still triggers a rescan. `data:` fonts decode inline (reusing
`decode_data_url`); network fonts load as `NetRequest::subresource`. Per the CSS
Fonts algorithm, each rule uses the **first** `src:` in declaration order whose
format we support (WOFF2/WOFF/TrueType/OpenType, or an unknown/absent
`format(...)` hint), skipping `local()` and explicitly-unsupported formats
(svg, embedded-opentype).

### D6. PDF text is subset + embedded (`subsetter`), not outlines

`export-pdf` replaces outline emission for TrueType runs with real text: glyphs
are grouped by `FontId`, shown as 2-byte CIDs of a `Type0`/`CIDFontType2` font
under `Identity-H`, positioned per glyph with a `[1,0,0,-1,ox,oy]` text matrix
(which re-flips y so glyphs are upright under the page's y-flip CTM; the font
size is applied by `Tf`). A run takes the embed path only when the sfnt is a
**single-face TrueType that parses and carries a `glyf` table**; CFF/OTF,
`.ttc` collections, and unparseable blobs fall back to outlines, which also
guarantees the font module never bails after the content stream has committed a
`/Font` reference (no dangling PDF objects). Each used font is subset to its
used glyphs with the
[`subsetter`](https://docs.rs/subsetter) crate (typst/pdf-writer ecosystem,
pure-Rust); the subset's compact gids are used directly as CIDs. Per font we
write the `FontFile2` (with `Length1`), a `CIDToGIDMap` stream, a `W` width
array (skrifa `GlyphMetrics` advances scaled to the PDF 1000-unit em), a
`FontDescriptor`, and a `ToUnicode` CMap. `subsetter` is added with
`default-features = false` (the `variable-fonts` feature, which pulls
`write-fonts`/`kurbo`, is not needed); `skrifa` becomes a direct `export-pdf`
dependency for the charmap and advances.

### D7. `ToUnicode` is a reverse-charmap (gid → codepoint) mapping

The `ToUnicode` CMap is built by reversing skrifa's `Charmap` (`mappings()`
yields `(codepoint, gid)`); the first codepoint mapping to each gid wins. CID →
codepoint then follows the subset's gid order. This makes text
selectable/extractable (verified end-to-end: `oxidepage pdf` of an `@font-face`
document yields `beginbfchar` entries mapping the run's CIDs back to the source
characters).

### D8. Reftests use `data:` font URIs

The web-font reftests (`webfont-woff2.html`/`webfont-woff.html` vs a shared
TTF-based `webfont-ref.html`) embed the fonts as `data:` URIs, mirroring the
existing image reftests. The secure-default `ResourcePolicy` blocks `file://`
subresources, so `data:` needs no harness/policy change. All three flavours
decode to identical sfnt outlines, so the renders are pixel-identical (fuzz
`0;0`). Test assets (`crates/layout/assets/webfont/{test.ttf,woff,woff2}`) are
generated offline from a from-scratch, real-glyph font (`generate.py`, using
`fonttools`+`brotli`), not Ahem's blank squares, so shaping is actually
exercised.

## Deviations from the design document / plan

- **`@font-face` discovery** walks author sheets' effective rules instead of
  `Stylist::iter_extra_data_origins()`, which is not externally iterable (D2).
- **`subsetter`** is used with `default-features = false` (no variable-font
  instancing), and a `CIDToGIDMap` **stream** is always written (identity in
  the subset case) so the subset-failure fallback — embedding the full font with
  CID → original-gid mapping — needs no conditional object allocation.

## v1 limitations

- **unicode-range** is captured but only approximated by the registered face's
  cmap coverage (fontique's charmap-based fallback already picks a covering
  face); it does not restrict a *full* font to a sub-range.
- **CORS for fonts:** loaded as `NetRequest::subresource` (NoCors); real
  browsers require CORS for cross-origin fonts. Deferred to the CORS work
  (§12 / Phase 10).
- **font-display ≈ block:** rendering waits for in-flight fonts (D4); the
  `swap`/`fallback`/`optional` timelines are not implemented.
- **ToUnicode** is reverse-charmap, so ligatures / many-to-one clusters may map
  imperfectly.
- **PDF CFF fonts:** only TrueType-flavored (`glyf`) fonts are subset/embedded
  as `CIDFontType2`/`FontFile2`; CFF/OTF (`OTTO`) runs fall back to the Phase 6
  vector-outline path (rendered, not selectable). A CFF (`FontFile3` /
  `CIDFontType0`) embed path is future work.
- **Variable fonts** are embedded as their default instance (no per-glyph
  variation coordinates in the PDF text path).
- **Distinct faces sharing one `src` URL:** two `@font-face` rules with the same
  family and the same `src` URL but different descriptors (e.g. a `400` and a
  `700` rule pointing at one file) dedup to a single registration (by
  `(family, url)` and `(family, blob-hash)`), so only the first rule's
  attributes take effect. Uncommon (variable fonts use one range rule; static
  weights normally ship separate files).
