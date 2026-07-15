# Web-font test assets (Phase 7, WP-A / WP-F)

`test.ttf`, `test.woff`, `test.woff2` are three flavours of the **same** tiny
font, used to test WOFF/WOFF2 decoding (`crates/layout/tests/webfont.rs`) and
the end-to-end web-font reftests (`tests/reftests/webfont-*`).

## Origin

The font is authored from scratch by `generate.py` in this directory — it does
**not** embed or derive from any third-party font, so it carries no license
encumbrance (public domain / same license as this repository). It defines real,
non-square glyph outlines (`A`, `F`, `O` — straight strokes plus quadratic
curves) so shaping and rasterization are actually exercised, unlike the blank
squares of the bundled Ahem font.

## Regenerating

Offline, with `fonttools` + `brotli` (already required for the stylo build):

```sh
pip install fonttools brotli
cd crates/layout/assets/webfont
python3 generate.py
```

`test.ttf` is raw sfnt (`0x00010000`), `test.woff` is WOFF1 (`wOFF`), and
`test.woff2` is WOFF2 (`wOF2`). All three carry identical glyph data: decoding
the WOFF flavours back to sfnt must reproduce `test.ttf`'s glyph **outlines**
(WOFF2 re-encodes the `glyf` table, so the raw bytes differ, but the decoded
outlines are identical — that is what the test asserts).
