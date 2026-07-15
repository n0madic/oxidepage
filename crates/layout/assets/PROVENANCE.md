# Font provenance

## Ahem.ttf

- Source: <https://raw.githubusercontent.com/web-platform-tests/wpt/master/fonts/Ahem.ttf>
  (the Web Platform Tests repository, `fonts/Ahem.ttf`).
- License: public domain (see the font's embedded license notice).
- Purpose: deterministic font metrics for layout tests. Every glyph is a
  1em × 1em square (ascent 0.8em, descent 0.2em), so text measurement is
  identical across platforms. The font is registered unconditionally by
  `FontSystem::new` and selected by tests via `font-family: Ahem`.
