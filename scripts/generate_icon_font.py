#!/usr/bin/env python3
"""Generate the self-hosted LiteOS system icon font and typed PUA mapping.

Run with the repository font environment:

    target/fontenv/bin/python scripts/generate_icon_font.py

Normal builds consume the checked TTF and generated TypeScript directly. The
JSON manifest is the only name/codepoint owner; this script owns outlines and
rejects a manifest icon that has no matching shape.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import tempfile
from pathlib import Path
from typing import Callable

try:
    from fontTools.fontBuilder import FontBuilder
    from fontTools.pens.ttGlyphPen import TTGlyphPen
    from fontTools.ttLib import TTFont
except ModuleNotFoundError as error:
    raise SystemExit(
        "regen-icon-font requires fontTools; normal builds consume checked assets"
    ) from error


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "assets/fonts/liteos-icons.json"
FONT_OUTPUT = ROOT / "assets/fonts/liteos-icons.ttf"
TYPESCRIPT_OUTPUT = ROOT / "ui/src/design-system/system-icons.generated.ts"
UNITS_PER_EM = 1024
ASCENT = 896
DESCENT = -128
FIXED_FONT_TIMESTAMP = 2082844800  # 1970-01-01 in OpenType's 1904 epoch.


def polygon(pen: TTGlyphPen, points: list[tuple[int, int]]) -> None:
    """Append one closed polygon contour to a TrueType glyph pen."""
    pen.moveTo(points[0])
    for point in points[1:]:
        pen.lineTo(point)
    pen.closePath()


def rectangle(pen: TTGlyphPen, left: int, bottom: int, right: int, top: int) -> None:
    """Append one axis-aligned rectangular contour."""
    polygon(pen, [(left, bottom), (right, bottom), (right, top), (left, top)])


def circle_points(
    center_x: int,
    center_y: int,
    radius: int,
    *,
    clockwise: bool,
    segments: int = 24,
) -> list[tuple[int, int]]:
    """Return a deterministic polygonal circle with the requested winding."""
    indexes = range(segments - 1, -1, -1) if clockwise else range(segments)
    return [
        (
            round(center_x + radius * math.cos(2 * math.pi * index / segments)),
            round(center_y + radius * math.sin(2 * math.pi * index / segments)),
        )
        for index in indexes
    ]


def chevron_right(pen: TTGlyphPen) -> None:
    polygon(pen, [(276, 718), (374, 806), (748, 512), (374, 218), (276, 306), (538, 512)])


def chevron_down(pen: TTGlyphPen) -> None:
    polygon(pen, [(218, 650), (306, 748), (512, 486), (718, 748), (806, 650), (512, 276)])


def sort_up(pen: TTGlyphPen) -> None:
    polygon(pen, [(512, 748), (246, 350), (778, 350)])


def sort_down(pen: TTGlyphPen) -> None:
    polygon(pen, [(246, 674), (778, 674), (512, 276)])


def playing(pen: TTGlyphPen) -> None:
    rectangle(pen, 220, 282, 350, 624)
    rectangle(pen, 447, 164, 577, 742)
    rectangle(pen, 674, 230, 804, 676)


def check(pen: TTGlyphPen) -> None:
    polygon(pen, [(164, 500), (254, 590), (426, 418), (770, 762), (860, 672), (426, 238)])


def search(pen: TTGlyphPen) -> None:
    # Opposite contour winding makes a true outline instead of a filled disk.
    polygon(pen, circle_points(430, 584, 268, clockwise=False))
    polygon(pen, circle_points(430, 584, 158, clockwise=True))
    polygon(pen, [(586, 370), (670, 454), (862, 262), (778, 178)])


SHAPES: dict[str, Callable[[TTGlyphPen], None]] = {
    "chevron-right": chevron_right,
    "chevron-down": chevron_down,
    "sort-up": sort_up,
    "sort-down": sort_down,
    "playing": playing,
    "check": check,
    "search": search,
}


def load_manifest() -> tuple[str, str, list[tuple[str, int]]]:
    """Validate and return family, CSS family, and ordered PUA entries."""
    raw = json.loads(MANIFEST.read_text())
    family = raw.get("family")
    css_family = raw.get("cssFamily")
    icons = raw.get("icons")
    if not isinstance(family, str) or not family:
        raise RuntimeError("icon manifest requires a non-empty family")
    if not isinstance(css_family, str) or not re.fullmatch(r"[a-z][a-z0-9-]*", css_family):
        raise RuntimeError("icon manifest cssFamily must be a lowercase CSS identifier")
    if not isinstance(icons, list) or not icons:
        raise RuntimeError("icon manifest requires at least one icon")
    entries: list[tuple[str, int]] = []
    for icon in icons:
        name = icon.get("name") if isinstance(icon, dict) else None
        encoded = icon.get("codepoint") if isinstance(icon, dict) else None
        if not isinstance(name, str) or not re.fullmatch(r"[a-z][a-z0-9-]*", name):
            raise RuntimeError(f"invalid icon name: {name!r}")
        if not isinstance(encoded, str) or not re.fullmatch(r"[0-9A-F]{4,6}", encoded):
            raise RuntimeError(f"invalid codepoint for {name}: {encoded!r}")
        codepoint = int(encoded, 16)
        if not 0xE000 <= codepoint <= 0xF8FF:
            raise RuntimeError(f"{name} must use the BMP Private Use Area")
        entries.append((name, codepoint))
    names = [name for name, _ in entries]
    codepoints = [codepoint for _, codepoint in entries]
    if len(set(names)) != len(names) or len(set(codepoints)) != len(codepoints):
        raise RuntimeError("icon names and PUA codepoints must be unique")
    if set(names) != set(SHAPES):
        raise RuntimeError(
            f"manifest/outline mismatch: manifest={sorted(names)}, outlines={sorted(SHAPES)}"
        )
    return family, css_family, entries


def build_font(path: Path, family: str, entries: list[tuple[str, int]]) -> None:
    """Build one deterministic TrueType face containing the manifest glyphs."""
    glyph_order = [".notdef", *(name.replace("-", "_") for name, _ in entries)]
    glyphs = {".notdef": TTGlyphPen(None).glyph()}
    for name, _ in entries:
        pen = TTGlyphPen(None)
        SHAPES[name](pen)
        glyphs[name.replace("-", "_")] = pen.glyph()
    metrics = {glyph: (UNITS_PER_EM, 0) for glyph in glyph_order}
    builder = FontBuilder(UNITS_PER_EM, isTTF=True)
    builder.setupGlyphOrder(glyph_order)
    builder.setupCharacterMap(
        {codepoint: name.replace("-", "_") for name, codepoint in entries}
    )
    builder.setupGlyf(glyphs)
    builder.setupHorizontalMetrics(metrics)
    builder.setupHorizontalHeader(ascent=ASCENT, descent=DESCENT)
    builder.setupNameTable(
        {
            "familyName": family,
            "styleName": "Regular",
            "uniqueFontIdentifier": "LiteOS Icons Regular 1.0",
            "fullName": f"{family} Regular",
            "psName": "LiteOS-Icons",
            "version": "Version 1.0",
        }
    )
    builder.setupOS2(
        sTypoAscender=ASCENT,
        sTypoDescender=DESCENT,
        sTypoLineGap=0,
        usWinAscent=ASCENT,
        usWinDescent=-DESCENT,
        usWeightClass=400,
        usWidthClass=5,
    )
    builder.setupPost(keepGlyphNames=True)
    builder.setupMaxp()
    builder.font.recalcTimestamp = False
    builder.font["head"].created = FIXED_FONT_TIMESTAMP
    builder.font["head"].modified = FIXED_FONT_TIMESTAMP
    builder.save(path)


def typescript(css_family: str, entries: list[tuple[str, int]]) -> str:
    """Return the generated TypeScript name-to-PUA mapping."""
    rows = "\n".join(
        f'  "{name}": "\\u{codepoint:04X}",' for name, codepoint in entries
    )
    return f'''// Generated by scripts/generate_icon_font.py; do not edit by hand.
export const SYSTEM_ICON_FAMILY = "{css_family}";

export const SYSTEM_ICON_GLYPHS = {{
{rows}
}} as const;

export type SystemIconName = keyof typeof SYSTEM_ICON_GLYPHS;
'''


def sha256(path: Path) -> str:
    """Return the lowercase SHA-256 identity of one generated artifact."""
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify_font(path: Path, family: str, entries: list[tuple[str, int]]) -> None:
    """Reparse a generated face and check its family and complete PUA cmap."""
    font = TTFont(path, lazy=True)
    actual_family = font["name"].getDebugName(16) or font["name"].getDebugName(1)
    if actual_family != family:
        raise RuntimeError(f"icon font family mismatch: {actual_family!r} != {family!r}")
    cmap = font.getBestCmap()
    expected = {codepoint: name.replace("-", "_") for name, codepoint in entries}
    if cmap != expected:
        raise RuntimeError(f"icon font cmap mismatch: {cmap!r} != {expected!r}")


def generate(verify: bool) -> None:
    """Generate or verify both checked artifacts transactionally."""
    family, css_family, entries = load_manifest()
    with tempfile.TemporaryDirectory(prefix="liteos-icon-font-") as directory:
        generated_font = Path(directory) / FONT_OUTPUT.name
        build_font(generated_font, family, entries)
        generated_typescript = typescript(css_family, entries)
        verify_font(generated_font, family, entries)
        if verify:
            if not FONT_OUTPUT.is_file() or FONT_OUTPUT.read_bytes() != generated_font.read_bytes():
                raise RuntimeError("checked icon font differs from manifest/outlines")
            if not TYPESCRIPT_OUTPUT.is_file() or TYPESCRIPT_OUTPUT.read_text() != generated_typescript:
                raise RuntimeError("checked TypeScript PUA mapping differs from manifest")
            print(
                f"verified {FONT_OUTPUT.relative_to(ROOT)}: {len(entries)} glyphs, "
                f"{FONT_OUTPUT.stat().st_size} bytes, sha256={sha256(FONT_OUTPUT)}"
            )
            return
        FONT_OUTPUT.write_bytes(generated_font.read_bytes())
        TYPESCRIPT_OUTPUT.write_text(generated_typescript)
        print(
            f"generated {FONT_OUTPUT.relative_to(ROOT)} and "
            f"{TYPESCRIPT_OUTPUT.relative_to(ROOT)}: {len(entries)} glyphs, "
            f"{FONT_OUTPUT.stat().st_size} bytes, sha256={sha256(FONT_OUTPUT)}"
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--verify", action="store_true", help="rebuild in memory and compare checked outputs")
    arguments = parser.parse_args()
    generate(arguments.verify)


if __name__ == "__main__":
    main()
