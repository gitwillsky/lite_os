#!/usr/bin/env python3
"""Generate the checked LiteOS terminal A8 atlas from fixed JetBrains Mono NL faces."""

from __future__ import annotations

import argparse
import hashlib
import struct
from pathlib import Path

try:
    from PIL import Image, ImageDraw, ImageFont
except ModuleNotFoundError as error:
    raise SystemExit("regen-font requires Pillow; normal builds consume the checked atlas") from error


ROOT = Path(__file__).resolve().parents[1]
MEDIUM = ROOT / "assets/fonts/JetBrainsMonoNL-Medium.ttf"
BOLD = ROOT / "assets/fonts/JetBrainsMonoNL-Bold.ttf"
OUTPUT = ROOT / "assets/fonts/liteos-terminal.a8"
MAGIC = b"LTA8\0\0\0\2"
CELL_WIDTH = 16
CELL_HEIGHT = 32
FACE_COUNT = 2
MEDIUM_SHA256 = "44099e1efefba55637e0abbbf8dd3f526e59523345888a257bb01d39df4af74c"
BOLD_SHA256 = "0198e841824025f8876e5c297f0b9b497ee8d6eb9969710a3328e1303f996ec3"


def sha256(path: Path) -> str:
    """Return the lowercase SHA-256 identity of one font/build artifact."""
    return hashlib.sha256(path.read_bytes()).hexdigest()


def codepoints() -> list[int]:
    """Return the sorted, duplicate-free terminal glyph contract."""
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
    return sorted(values)


def render_face(path: Path, width: int, height: int, pixel_size: int, glyphs: list[int]) -> bytes:
    """Rasterize one fixed-cell face as tightly packed row-major A8 glyphs."""
    font = ImageFont.truetype(path, pixel_size, layout_engine=ImageFont.Layout.BASIC)
    ascent, descent = font.getmetrics()
    baseline = (height - ascent - descent) // 2 + ascent
    rendered = bytearray()
    for codepoint in glyphs:
        image = Image.new("L", (width, height), 0)
        draw = ImageDraw.Draw(image)
        draw.text(
            (width // 2, baseline),
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
    glyphs = codepoints()
    faces = (
        render_face(medium, CELL_WIDTH, CELL_HEIGHT, 24, glyphs),
        render_face(bold, CELL_WIDTH, CELL_HEIGHT, 24, glyphs),
    )
    header = bytearray(32)
    header[:8] = MAGIC
    struct.pack_into("<I", header, 8, len(glyphs))
    struct.pack_into("<I", header, 12, len(header))
    struct.pack_into("<I", header, 16, len(header) + len(glyphs) * 4)
    struct.pack_into("<HHI", header, 20, CELL_WIDTH, CELL_HEIGHT, FACE_COUNT)
    payload = bytes(header) + b"".join(struct.pack("<I", value) for value in glyphs) + b"".join(faces)
    if len(payload) > 2 * 1024 * 1024:
        raise RuntimeError(f"terminal atlas exceeds 2 MiB contract: {len(payload)} bytes")
    temporary = output.with_suffix(output.suffix + ".tmp")
    temporary.write_bytes(payload)
    temporary.replace(output)
    print(
        f"generated {output.relative_to(ROOT)}: {len(glyphs)} glyphs, "
        f"{len(payload)} bytes, sha256={hashlib.sha256(payload).hexdigest()}"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--medium", type=Path, default=MEDIUM)
    parser.add_argument("--bold", type=Path, default=BOLD)
    parser.add_argument("--output", type=Path, default=OUTPUT)
    arguments = parser.parse_args()
    generate(arguments.medium, arguments.bold, arguments.output)


if __name__ == "__main__":
    main()
