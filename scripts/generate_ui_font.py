#!/usr/bin/env python3
"""Generate the checked LiteOS UI proportional A8 atlas from pinned Noto Sans CJK SC faces.

Run with the pinned host interpreter: target/fontenv/bin/python scripts/generate_ui_font.py
Normal builds consume the checked atlas and need neither Pillow nor the OTF sources.
"""

from __future__ import annotations

import argparse
import hashlib
import struct
import urllib.request
from pathlib import Path

try:
    from PIL import Image, ImageDraw, ImageFont
except ModuleNotFoundError as error:
    raise SystemExit("regen-ui-font requires Pillow; normal builds consume the checked atlas") from error


ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / "target/font-cache"
REGULAR = CACHE / "NotoSansCJKsc-Regular.otf"
BOLD = CACHE / "NotoSansCJKsc-Bold.otf"
OUTPUT = ROOT / "assets/fonts/liteos-ui.a8p"
MAGIC = b"LUP8\0\0\0\x01"
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
# face_kind 0 = regular, 1 = bold; both kinds are rendered at every pixel size.
FACES = (
    (0, REGULAR, REGULAR_URL, REGULAR_SHA256),
    (1, BOLD, BOLD_URL, BOLD_SHA256),
)
PIXEL_SIZES = (13, 16)
# Desktop asset budget; exceeding it means the glyph set or sizes must shrink.
MAX_BYTES = 6 * 1024 * 1024


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


def render_face(path: Path, pixel_size: int, glyphs: list[int]) -> tuple[bytes, int, int, int, int]:
    """Rasterize one proportional face as tightly packed row-major A8 glyphs.

    Anchor "ls" places the origin at the left baseline, so a consumer blits the
    bitmap at (pen_x + xoff, baseline_y + yoff) and advances by `advance`.
    Returns (payload, ascent, descent, empty_bitmaps, bitmap_bytes).
    """
    font = ImageFont.truetype(path, pixel_size, layout_engine=ImageFont.Layout.BASIC)
    ascent, descent = font.getmetrics()
    payload = bytearray()
    empty = 0
    bitmap_bytes = 0
    for codepoint in glyphs:
        char = chr(codepoint)
        advance = int(round(font.getlength(char)))
        x0, y0, x1, y1 = font.getbbox(char, anchor="ls")
        width = max(0, x1 - x0)
        height = max(0, y1 - y0)
        payload.extend(struct.pack("<hhhHH", advance, x0, y0, width, height))
        if width == 0 or height == 0:
            empty += 1
            continue
        image = Image.new("L", (width, height), 0)
        ImageDraw.Draw(image).text((-x0, -y0), char, font=font, fill=255, anchor="ls")
        payload.extend(image.tobytes())
        bitmap_bytes += width * height
    return bytes(payload), ascent, descent, empty, bitmap_bytes


def generate(output: Path) -> None:
    """Write one transactional atlas consumed directly by the no_std desktop."""
    for _, path, url, expected in FACES:
        ensure_font(path, url, expected)
    glyphs = codepoints()
    payload = bytearray()
    payload.extend(MAGIC)
    payload.extend(struct.pack("<I", len(FACES) * len(PIXEL_SIZES)))
    payload.extend(struct.pack("<I", len(glyphs)))
    payload.extend(b"".join(struct.pack("<I", value) for value in glyphs))
    for face_kind, path, _, _ in FACES:
        for pixel_size in PIXEL_SIZES:
            face, ascent, descent, empty, bitmap_bytes = render_face(path, pixel_size, glyphs)
            payload.extend(struct.pack("<IIii", face_kind, pixel_size, ascent, descent))
            payload.extend(face)
            print(
                f"face kind={face_kind} size={pixel_size}px: ascent={ascent} descent={descent}, "
                f"{len(glyphs)} glyphs ({empty} empty bitmaps), {bitmap_bytes} bitmap bytes"
            )
    if len(payload) > MAX_BYTES:
        raise RuntimeError(f"UI atlas exceeds {MAX_BYTES} byte budget: {len(payload)} bytes")
    temporary = output.with_suffix(output.suffix + ".tmp")
    temporary.write_bytes(bytes(payload))
    temporary.replace(output)
    print(
        f"generated {output.relative_to(ROOT)}: {len(glyphs)} glyphs, "
        f"{len(payload)} bytes, sha256={hashlib.sha256(bytes(payload)).hexdigest()}"
    )


def verify(path: Path) -> None:
    """Reparse a generated atlas and check its structural invariants."""
    data = path.read_bytes()
    if data[:8] != MAGIC:
        raise RuntimeError(f"bad magic: {data[:8]!r}")
    face_count, glyph_count = struct.unpack_from("<II", data, 8)
    offset = 16
    glyphs = [value[0] for value in struct.iter_unpack("<I", data[offset : offset + glyph_count * 4])]
    if any(a >= b for a, b in zip(glyphs, glyphs[1:])):
        raise RuntimeError("glyph table is not strictly increasing")
    if 0xFFFD not in glyphs:
        raise RuntimeError("U+FFFD fallback is missing from the glyph table")
    offset += glyph_count * 4
    index = {value: position for position, value in enumerate(glyphs)}
    samples = [index[value] for value in (0x4E2D, 0x6587, 0x684C, 0x9762, 0xFFFD)]
    nonempty = 0
    for face in range(face_count):
        face_kind, pixel_size, ascent, descent = struct.unpack_from("<IIii", data, offset)
        offset += 16
        if face_kind not in (0, 1):
            raise RuntimeError(f"face {face}: bad kind {face_kind}")
        if ascent <= 0 or descent < 0:
            raise RuntimeError(f"face {face}: bad metrics ascent={ascent} descent={descent}")
        for glyph in range(glyph_count):
            advance, xoff, yoff, width, height = struct.unpack_from("<hhhHH", data, offset)
            offset += 10 + width * height
            if offset > len(data):
                raise RuntimeError(f"face {face} glyph {glyph}: record runs past end of file")
            if glyph in samples and width * height > 0:
                nonempty += 1
        print(f"verified face kind={face_kind} size={pixel_size}px: ascent={ascent} descent={descent}")
    if offset != len(data):
        raise RuntimeError(f"trailing bytes: parsed {offset}, file is {len(data)}")
    if nonempty != len(samples) * face_count:
        raise RuntimeError(f"sample bitmaps missing: {nonempty} of {len(samples) * face_count} nonempty")
    print(
        f"verified {path.relative_to(ROOT)}: {face_count} faces, {glyph_count} glyphs, "
        f"{len(data)} bytes, sampled bitmaps nonempty"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, default=OUTPUT)
    parser.add_argument("--verify", action="store_true", help="reparse and check the atlas instead of generating")
    arguments = parser.parse_args()
    if arguments.verify:
        verify(arguments.output)
    else:
        generate(arguments.output)


if __name__ == "__main__":
    main()
