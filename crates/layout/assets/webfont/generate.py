#!/usr/bin/env python3
"""Generate the Phase 7 web-font test assets.

Builds a tiny, public-domain test font with *real* (non-square) glyph outlines
and exports it as raw sfnt (`test.ttf`), WOFF1 (`test.woff`), and WOFF2
(`test.woff2`). The three files carry identical glyph data, so decoding the
WOFF/WOFF2 flavours back to sfnt must reproduce `test.ttf`'s outlines
byte-for-byte (see `crates/layout/tests/webfont.rs`).

The font is authored here from scratch (no third-party font is embedded), so it
is unencumbered. Requirements (offline): `pip install fonttools brotli`.

Run from this directory: `python3 generate.py`.
"""

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen

UPM = 1000
ASCENT = 800
DESCENT = -200

# Real glyph outlines (not Ahem's blank squares) so shaping/rasterization is
# actually exercised. Coordinates in font units (y up).
def draw_A(pen):
    # A filled triangle with a rectangular counter (tests non-zero winding).
    pen.moveTo((50, 0))
    pen.lineTo((300, 700))
    pen.lineTo((550, 0))
    pen.closePath()
    # Inner counter (reverse direction).
    pen.moveTo((250, 150))
    pen.lineTo((350, 150))
    pen.lineTo((300, 400))
    pen.closePath()


def draw_F(pen):
    # An "F" stroke path — several straight segments, clearly non-square.
    pen.moveTo((100, 0))
    pen.lineTo((250, 0))
    pen.lineTo((250, 300))
    pen.lineTo((450, 300))
    pen.lineTo((450, 430))
    pen.lineTo((250, 430))
    pen.lineTo((250, 560))
    pen.lineTo((500, 560))
    pen.lineTo((500, 700))
    pen.lineTo((100, 700))
    pen.closePath()


def draw_O(pen):
    # An "O" with an outer and inner quadratic-curve contour (tests curves and
    # the counter fill).
    pen.moveTo((100, 350))
    pen.qCurveTo((100, 700), (350, 700))
    pen.qCurveTo((600, 700), (600, 350))
    pen.qCurveTo((600, 0), (350, 0))
    pen.qCurveTo((100, 0), (100, 350))
    pen.closePath()
    pen.moveTo((250, 350))
    pen.qCurveTo((250, 150), (350, 150))
    pen.qCurveTo((450, 150), (450, 350))
    pen.qCurveTo((450, 550), (350, 550))
    pen.qCurveTo((250, 550), (250, 350))
    pen.closePath()


GLYPHS = {
    "A": (draw_A, 600),
    "F": (draw_F, 600),
    "O": (draw_O, 700),
}


def build():
    glyph_order = [".notdef", "space"] + list(GLYPHS.keys())
    fb = FontBuilder(UPM, isTTF=True)
    fb.setupGlyphOrder(glyph_order)
    fb.setupCharacterMap(
        {0x20: "space", **{ord(ch): ch for ch in GLYPHS}}
    )

    pens = {}
    # .notdef and space are empty (blank), the letters have real outlines.
    empty = TTGlyphPen(None)
    pens[".notdef"] = empty.glyph()
    empty2 = TTGlyphPen(None)
    pens["space"] = empty2.glyph()
    advances = {".notdef": 500, "space": 500}
    for ch, (draw, adv) in GLYPHS.items():
        pen = TTGlyphPen(None)
        draw(pen)
        pens[ch] = pen.glyph()
        advances[ch] = adv

    fb.setupGlyf(pens)
    metrics = {}
    glyf = fb.font["glyf"]
    for name in glyph_order:
        bounds = glyf[name].xMin if hasattr(glyf[name], "xMin") else 0
        metrics[name] = (advances[name], bounds or 0)
    fb.setupHorizontalMetrics(metrics)
    fb.setupHorizontalHeader(ascent=ASCENT, descent=DESCENT)
    fb.setupNameTable(
        {
            "familyName": "OxideWebFontTest",
            "styleName": "Regular",
            "psName": "OxideWebFontTest-Regular",
        }
    )
    fb.setupOS2(sTypoAscender=ASCENT, sTypoDescender=DESCENT, usWinAscent=ASCENT, usWinDescent=-DESCENT)
    fb.setupPost()
    return fb


def main():
    fb = build()
    fb.save("test.ttf")

    fb.font.flavor = "woff"
    fb.save("test.woff")

    fb.font.flavor = "woff2"
    fb.save("test.woff2")

    print("wrote test.ttf, test.woff, test.woff2")


if __name__ == "__main__":
    main()
