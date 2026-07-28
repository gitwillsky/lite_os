#!/usr/bin/env python3
"""Generate the checked LiteOS terminal A8 atlas from fixed Latin and CJK faces.

Run with the pinned host interpreter: target/fontenv/bin/python scripts/generate_terminal_font.py
Normal builds consume the checked atlas and need neither Pillow nor the font sources.
"""

from __future__ import annotations

import argparse
import hashlib
import struct
from pathlib import Path

try:
    from PIL import Image, ImageDraw, ImageFont
except ModuleNotFoundError as error:
    raise SystemExit("regen-font requires Pillow; normal builds consume the checked atlas") from error

from generate_ui_font import (
    BOLD as CJK_BOLD,
    BOLD_SHA256 as CJK_BOLD_SHA256,
    BOLD_URL as CJK_BOLD_URL,
    REGULAR as CJK_REGULAR,
    REGULAR_SHA256 as CJK_REGULAR_SHA256,
    REGULAR_URL as CJK_REGULAR_URL,
    codepoints as cjk_codepoints,
    ensure_font,
)


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
MAX_BYTES = 10 * 1024 * 1024


def sha256(path: Path) -> str:
    """Return the lowercase SHA-256 identity of one font/build artifact."""
    return hashlib.sha256(path.read_bytes()).hexdigest()


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
    """Return the sorted terminal glyph contract including GB2312 level-1 Chinese."""
    return sorted(primary_codepoints() | set(cjk_codepoints()))


def render_face(primary_path: Path, cjk_path: Path, glyphs: list[int]) -> bytes:
    """Rasterize one fixed-cell face with a pinned Noto CJK fallback."""
    primary = ImageFont.truetype(
        primary_path,
        PRIMARY_PIXEL_SIZE,
        layout_engine=ImageFont.Layout.BASIC,
    )
    cjk = ImageFont.truetype(
        cjk_path,
        CJK_PIXEL_SIZE,
        layout_engine=ImageFont.Layout.BASIC,
    )
    metrics = {
        True: primary.getmetrics(),
        False: cjk.getmetrics(),
    }
    primary_glyphs = primary_codepoints()
    rendered = bytearray()
    for codepoint in glyphs:
        is_primary = codepoint in primary_glyphs
        font = primary if is_primary else cjk
        ascent, descent = metrics[is_primary]
        baseline = (CELL_HEIGHT - ascent - descent) // 2 + ascent
        image = Image.new("L", (BITMAP_WIDTH, CELL_HEIGHT), 0)
        draw = ImageDraw.Draw(image)
        center = CELL_WIDTH // 2 if is_primary else BITMAP_WIDTH // 2
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
    """Reparse the atlas and prove representative Chinese glyphs are real bitmaps."""
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
    face_bytes = glyph_count * BITMAP_WIDTH * CELL_HEIGHT
    for face in range(FACE_COUNT):
        base = faces + face * face_bytes
        for character, index in zip("中文乱码", samples):
            start = base + index * BITMAP_WIDTH * CELL_HEIGHT
            bitmap = data[start : start + BITMAP_WIDTH * CELL_HEIGHT]
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
    print(
        f"verified {path.relative_to(ROOT)}: {FACE_COUNT} faces, {glyph_count} glyphs, "
        f"{len(data)} bytes, Chinese samples nonempty"
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
