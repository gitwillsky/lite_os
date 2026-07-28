#!/usr/bin/env python3
"""Generate the checked LiteOS UI proportional OpenType faces from pinned Noto Sans CJK SC sources.

Run with the pinned host interpreter: target/fontenv/bin/python scripts/generate_ui_font.py
Normal builds consume the checked faces and need neither fontTools nor the OTF sources.

The desktop renderer rasterizes glyphs at runtime (parley shaping + swash), so
the product asset is the subsetted font itself, not a pre-rendered atlas. Each
face keeps its CFF outlines (`.otf` sfnt version) and is subset to the UI glyph
contract so the CJK table set stays inside the desktop asset budget.
"""

from __future__ import annotations

import argparse
import hashlib
import urllib.request
from pathlib import Path

try:
    from fontTools import subset
    from fontTools.ttLib import TTFont
except ModuleNotFoundError as error:
    raise SystemExit("regen-ui-font requires fontTools; normal builds consume the checked faces") from error


ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / "target/font-cache"
REGULAR = CACHE / "NotoSansCJKsc-Regular.otf"
BOLD = CACHE / "NotoSansCJKsc-Bold.otf"
REGULAR_OUTPUT = ROOT / "assets/fonts/liteos-ui-regular.otf"
BOLD_OUTPUT = ROOT / "assets/fonts/liteos-ui-bold.otf"
# SHA-256 of the pinned notofonts/noto-cjk main OTF sources, measured at fetch time.
REGULAR_SHA256 = "2c76254f6fc379fddfce0a7e84fb5385bb135d3e399294f6eeb6680d0365b74b"
BOLD_SHA256 = "b5f0d1a190a7f9b43c310a8850630af12553df32c4c050543f9059732d9b4c0a"
REGULAR_URL = (
    "https://raw.githubusercontent.com/notofonts/noto-cjk/main/"
    "Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Regular.otf"
)
BOLD_URL = (
    "https://raw.githubusercontent.com/notofonts/noto-cjk/main/"
    "Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Bold.otf"
)
# (字体文件, URL, sha256, 输出)。regular 承担正文/菜单，bold 承担标题栏/强调；
# font-weight 中间档由 fontique 按 CSS Fonts 匹配规则回落到这两档之一。
FACES = (
    (REGULAR, REGULAR_URL, REGULAR_SHA256, REGULAR_OUTPUT),
    (BOLD, BOLD_URL, BOLD_SHA256, BOLD_OUTPUT),
)
# Desktop asset budget per face; exceeding it means the glyph set must shrink.
MAX_BYTES = 2 * 1024 * 1024


def sha256(path: Path) -> str:
    """Return the lowercase SHA-256 identity of one font/build artifact."""
    return hashlib.sha256(path.read_bytes()).hexdigest()


def ensure_font(path: Path, url: str, expected: str) -> None:
    """Fetch one pinned OTF into the cache; a matching cached copy skips the network."""
    if path.exists():
        actual = sha256(path)
        if actual == expected:
            return
        raise RuntimeError(f"cached font identity mismatch: {path}: expected {expected}, got {actual}")
    try:
        with urllib.request.urlopen(url) as response:
            data = response.read()
    except OSError as error:
        raise SystemExit(f"font download failed and cache is missing: {path}: {error}") from error
    actual = hashlib.sha256(data).hexdigest()
    if actual != expected:
        raise RuntimeError(f"downloaded font identity mismatch: {url}: expected {expected}, got {actual}")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def codepoints() -> list[int]:
    """Return the sorted, duplicate-free UI glyph contract.

    1. ASCII 0x20-0x7E plus U+FFFD fallback.
    2. GB2312 level-1 hanzi (0xB0A1-0xD7F9, 3755 codepoints).
    3. GB2312 symbol rows 0xA1A1-0xA2FE and 0xA3A1-0xA3FE; undefined slots
       raise UnicodeDecodeError in the gb2312 codec and are skipped.
    """
    values = set(range(0x20, 0x7F)) | {0xFFFD}
    # Rows 0xB0-0xD6 are full; row 0xD7 stops at trail byte 0xF9 (3755 hanzi total).
    for first in range(0xB0, 0xD8):
        last = 0xF9 if first == 0xD7 else 0xFE
        for second in range(0xA1, last + 1):
            values.update(ord(char) for char in bytes((first, second)).decode("gb2312"))
    for first in (0xA1, 0xA2, 0xA3):
        for second in range(0xA1, 0xFF):
            try:
                values.update(ord(char) for char in bytes((first, second)).decode("gb2312"))
            except UnicodeDecodeError:
                continue
    return sorted(values)


def subset_face(source: Path, output: Path, glyphs: list[int]) -> None:
    """Subset one pinned OTF to the UI glyph contract, keeping CFF outlines.

    Hinting is dropped: the renderer rasterizes at arbitrary sizes where CFF
    stems at 11-14 logical px gain little, and the hint table is a large share
    of the CJK charstrings. The name and OS/2 tables stay intact so fontique
    reads the family name and weight class for CSS matching.
    """
    options = subset.Options()
    options.hinting = False
    options.name_IDs = ["*"]
    font = subset.load_font(str(source), options)
    subsetter = subset.Subsetter(options)
    subsetter.populate(unicodes=glyphs)
    subsetter.subset(font)
    temporary = output.with_suffix(output.suffix + ".tmp")
    font.save(temporary)
    if temporary.stat().st_size > MAX_BYTES:
        temporary.unlink()
        raise RuntimeError(f"{output.name} exceeds {MAX_BYTES} byte budget: {temporary.stat().st_size} bytes")
    temporary.replace(output)
    print(
        f"generated {output.relative_to(ROOT)}: {len(glyphs)} codepoints, "
        f"{output.stat().st_size} bytes, sha256={sha256(output)}"
    )


def generate() -> None:
    """Write both transactional faces consumed directly by the desktop."""
    for path, url, expected, output in FACES:
        ensure_font(path, url, expected)
        subset_face(path, output, codepoints())


def verify(path: Path) -> None:
    """Reparse a generated face and check its structural invariants."""
    font = TTFont(path, lazy=True)
    if font.sfntVersion != "OTTO":
        raise RuntimeError(f"expected CFF OpenType (OTTO), got {font.sfntVersion!r}")
    cmap = font.getBestCmap()
    # U+FFFD has no cmap entry in the Noto sources; the renderer shapes it to
    # the font's .notdef box, which is the intended fallback glyph anyway.
    for sample in (0x20, 0x4E2D, 0x6587, 0x684C, 0x9762, 0x2026):
        if sample not in cmap:
            raise RuntimeError(f"U+{sample:04X} is missing from the cmap")
    weight = font["OS/2"].usWeightClass
    family = font["name"].getDebugName(16) or font["name"].getDebugName(1)
    if not family:
        raise RuntimeError("family name is missing from the name table")
    glyph_count = len(font.getGlyphOrder())
    print(
        f"verified {path.relative_to(ROOT)}: family={family!r} weight={weight}, "
        f"{glyph_count} glyphs, {path.stat().st_size} bytes"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, default=None, help="unused legacy argument")
    parser.add_argument("--verify", action="store_true", help="reparse and check both faces instead of generating")
    arguments = parser.parse_args()
    if arguments.verify:
        verify(REGULAR_OUTPUT)
        verify(BOLD_OUTPUT)
    else:
        generate()


if __name__ == "__main__":
    main()
