#!/usr/bin/env python3
"""Generate the checked LiteOS terminal A8 atlas from fixed Latin, CJK and symbol faces.

Run with the pinned host interpreter: .local/tools/classic-assets/bin/python scripts/generate_terminal_font.py
Normal builds consume the checked atlas and need neither Pillow nor the font sources.
"""

from __future__ import annotations

import argparse
import hashlib
import struct
import urllib.request
from pathlib import Path

from local_inputs import INPUTS

ROOT = Path(__file__).resolve().parents[1]
MEDIUM = ROOT / "assets/fonts/JetBrainsMonoNL-Medium.ttf"
BOLD = ROOT / "assets/fonts/JetBrainsMonoNL-Bold.ttf"
OUTPUT = ROOT / "assets/fonts/liteos-terminal.a8"
MAGIC = b"LTA8\0\0\0\3"
CELL_WIDTH = 16
CELL_HEIGHT = 32
BITMAP_WIDTH = 32
FACE_COUNT = 2
MEDIUM_SHA256 = "44099e1efefba55637e0abbbf8dd3f526e59523345888a257bb01d39df4af74c"
BOLD_SHA256 = "0198e841824025f8876e5c297f0b9b497ee8d6eb9969710a3328e1303f996ec3"
PRIMARY_PIXEL_SIZE = 24
CJK_PIXEL_SIZE = 24
SYMBOL_PIXEL_SIZE = 20
MAX_BYTES = 10 * 1024 * 1024


def sha256(path: Path) -> str:
    """Return the lowercase SHA-256 identity of one font/build artifact."""
    return hashlib.sha256(path.read_bytes()).hexdigest()


CJK_REGULAR = INPUTS / "fonts/NotoSansCJKsc-Regular.otf"
CJK_BOLD = INPUTS / "fonts/NotoSansCJKsc-Bold.otf"
SYMBOLS = INPUTS / "fonts/NotoSansSymbols2-Regular.ttf"

CJK_REGULAR_SHA256 = "2c76254f6fc379fddfce0a7e84fb5385bb135d3e399294f6eeb6680d0365b74b"

CJK_BOLD_SHA256 = "b5f0d1a190a7f9b43c310a8850630af12553df32c4c050543f9059732d9b4c0a"
SYMBOLS_SHA256 = "7d5fb73b7ca67a6798101741f5d280a3d016a56a197afcd4199dbb57b4b82a21"

CJK_REGULAR_URL = (
    "https://raw.githubusercontent.com/notofonts/noto-cjk/main/"
    "Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Regular.otf"
)

CJK_BOLD_URL = (
    "https://raw.githubusercontent.com/notofonts/noto-cjk/main/"
    "Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Bold.otf"
)
SYMBOLS_URL = (
    "https://raw.githubusercontent.com/google/fonts/7b6724ac7ececc713e9ba93af309f7520c9a80a3/"
    "ofl/notosanssymbols2/NotoSansSymbols2-Regular.ttf"
)

# Unicode 17.0 §21.1.1. The Latin and CJK faces omit this block, so they
# cannot supply glyphs for Braille-based progress indicators and plots.
BRAILLE = range(0x2800, 0x2900)

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

def cjk_codepoints() -> list[int]:
    """Return the sorted, duplicate-free CJK terminal glyph contract.

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

def primary_codepoints() -> set[int]:
    """Return glyphs rendered by JetBrains Mono rather than the CJK fallback."""
    values = {
        *range(0x20, 0x7F),
        *range(0xA0, 0x100),
        *range(0x2190, 0x2200),
        *range(0x2500, 0x2580),
        *range(0x2580, 0x25A0),
        0x25B2,
        0x25B3,
        0x25BC,
        0x25BD,
        0xFFFD,
    }
    return values


def codepoints() -> list[int]:
    """Return the sorted union of the selected terminal font repertoires."""
    return sorted(primary_codepoints() | set(cjk_codepoints()) | set(BRAILLE))


def render_face(primary_path: Path, cjk_path: Path, glyphs: list[int]) -> bytes:
    """Rasterize one atlas face using the first source assigned to each codepoint."""
    try:
        from PIL import Image, ImageDraw, ImageFont
    except ModuleNotFoundError as error:
        raise SystemExit("regen-font requires Pillow; --verify only needs Python") from error

    sources = (
        (primary_path, PRIMARY_PIXEL_SIZE, CELL_WIDTH, primary_codepoints()),
        (cjk_path, CJK_PIXEL_SIZE, BITMAP_WIDTH, cjk_codepoints()),
        # Noto Sans Symbols 2 has one weight. Reuse it for both atlas faces.
        (SYMBOLS, SYMBOL_PIXEL_SIZE, CELL_WIDTH, BRAILLE),
    )
    glyph_sources = {}
    for path, size, width, repertoire in sources:
        font = ImageFont.truetype(path, size, layout_engine=ImageFont.Layout.BASIC)
        ascent, descent = font.getmetrics()
        baseline = (CELL_HEIGHT - ascent - descent) // 2 + ascent
        for codepoint in repertoire:
            glyph_sources.setdefault(codepoint, (font, width // 2, baseline))
    rendered = bytearray()
    for codepoint in glyphs:
        font, center, baseline = glyph_sources[codepoint]
        image = Image.new("L", (BITMAP_WIDTH, CELL_HEIGHT), 0)
        draw = ImageDraw.Draw(image)
        draw.text(
            (center, baseline),
            chr(codepoint),
            font=font,
            fill=255,
            anchor="ms",
            embedded_color=False,
        )
        rendered.extend(image.tobytes())
    return bytes(rendered)


def generate(medium: Path, bold: Path, output: Path) -> None:
    """Write one transactional atlas consumed directly by the terminal."""
    for path, expected in ((medium, MEDIUM_SHA256), (bold, BOLD_SHA256)):
        actual = sha256(path)
        if actual != expected:
            raise RuntimeError(f"font identity mismatch: {path}: expected {expected}, got {actual}")
    ensure_font(CJK_REGULAR, CJK_REGULAR_URL, CJK_REGULAR_SHA256)
    ensure_font(CJK_BOLD, CJK_BOLD_URL, CJK_BOLD_SHA256)
    ensure_font(SYMBOLS, SYMBOLS_URL, SYMBOLS_SHA256)
    glyphs = codepoints()
    faces = (
        render_face(medium, CJK_REGULAR, glyphs),
        render_face(bold, CJK_BOLD, glyphs),
    )
    header = bytearray(32)
    header[:8] = MAGIC
    struct.pack_into("<I", header, 8, len(glyphs))
    struct.pack_into("<I", header, 12, len(header))
    struct.pack_into("<I", header, 16, len(header) + len(glyphs) * 4)
    struct.pack_into("<HHI", header, 20, CELL_WIDTH, CELL_HEIGHT, FACE_COUNT)
    struct.pack_into("<H", header, 28, BITMAP_WIDTH)
    payload = bytes(header) + b"".join(struct.pack("<I", value) for value in glyphs) + b"".join(faces)
    if len(payload) > MAX_BYTES:
        raise RuntimeError(f"terminal atlas exceeds {MAX_BYTES} byte contract: {len(payload)} bytes")
    temporary = output.with_suffix(output.suffix + ".tmp")
    temporary.write_bytes(payload)
    temporary.replace(output)
    print(
        f"generated {output.relative_to(ROOT)}: {len(glyphs)} glyphs, "
        f"{len(payload)} bytes, sha256={hashlib.sha256(payload).hexdigest()}"
    )


def verify(path: Path) -> None:
    """Check the atlas layout and the stored CJK and Braille pixel coverage."""
    data = path.read_bytes()
    if data[:8] != MAGIC:
        raise RuntimeError(f"bad magic: {data[:8]!r}")
    glyph_count, codepoints_offset, faces = struct.unpack_from("<III", data, 8)
    expected = codepoints()
    glyphs = [
        value[0]
        for value in struct.iter_unpack(
            "<I",
            data[codepoints_offset : codepoints_offset + glyph_count * 4],
        )
    ]
    if glyphs != expected:
        raise RuntimeError("terminal glyph table does not match the fixed contract")
    if struct.unpack_from("<HHIH", data, 20) != (
        CELL_WIDTH,
        CELL_HEIGHT,
        FACE_COUNT,
        BITMAP_WIDTH,
    ):
        raise RuntimeError("terminal atlas geometry changed")
    expected_size = faces + FACE_COUNT * glyph_count * BITMAP_WIDTH * CELL_HEIGHT
    if faces != codepoints_offset + glyph_count * 4 or len(data) != expected_size:
        raise RuntimeError("terminal atlas size is invalid")
    samples = [glyphs.index(ord(character)) for character in "中文乱码"]
    glyph_bytes = BITMAP_WIDTH * CELL_HEIGHT
    face_bytes = glyph_count * glyph_bytes
    braille_offset = glyphs.index(BRAILLE.start) * glyph_bytes
    for face in range(FACE_COUNT):
        base = faces + face * face_bytes
        for character, index in zip("中文乱码", samples):
            start = base + index * glyph_bytes
            bitmap = data[start : start + glyph_bytes]
            if not any(bitmap):
                raise RuntimeError(f"terminal face {face} has an empty {character!r} glyph")
            if not any(
                alpha
                for row in range(CELL_HEIGHT)
                for alpha in bitmap[
                    row * BITMAP_WIDTH + CELL_WIDTH : (row + 1) * BITMAP_WIDTH
                ]
            ):
                raise RuntimeError(
                    f"terminal face {face} keeps {character!r} inside one narrow cell"
                )
        start = base + braille_offset
        braille = [
            data[offset : offset + glyph_bytes]
            for offset in range(start, start + len(BRAILLE) * glyph_bytes, glyph_bytes)
        ]
        if any(braille[0]) or len(set(braille)) != len(BRAILLE):
            raise RuntimeError(f"terminal face {face} has missing or duplicate Braille glyphs")
        if any(
            any(bitmap[row + CELL_WIDTH : row + BITMAP_WIDTH])
            for bitmap in braille
            for row in range(0, glyph_bytes, BITMAP_WIDTH)
        ):
            raise RuntimeError(f"terminal face {face} spills Braille outside one cell")
    print(
        f"verified {path.relative_to(ROOT)}: {FACE_COUNT} faces, {glyph_count} glyphs, "
        f"{len(data)} bytes, Chinese samples nonempty, {len(BRAILLE)} distinct Braille glyphs per face"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--medium", type=Path, default=MEDIUM)
    parser.add_argument("--bold", type=Path, default=BOLD)
    parser.add_argument("--output", type=Path, default=OUTPUT)
    parser.add_argument("--verify", action="store_true", help="reparse the checked atlas")
    arguments = parser.parse_args()
    if arguments.verify:
        verify(arguments.output)
    else:
        generate(arguments.medium, arguments.bold, arguments.output)


if __name__ == "__main__":
    main()
