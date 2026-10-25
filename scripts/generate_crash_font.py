#!/usr/bin/env python3
"""Generate the kernel blue-screen 8x16 ASCII bitmap font from the checked JetBrains Mono face.

Run with the pinned host interpreter: .local/tools/classic-assets/bin/python scripts/generate_crash_font.py
Normal builds consume the checked `kernel/src/crash/font.rs` and need neither Pillow nor this script.
"""

from __future__ import annotations

import hashlib
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
FACE = ROOT / "assets/fonts/JetBrainsMonoNL-Bold.ttf"
FACE_SHA256 = "0198e841824025f8876e5c297f0b9b497ee8d6eb9969710a3328e1303f996ec3"
OUTPUT = ROOT / "kernel/src/crash/font.rs"
WIDTH = 8
HEIGHT = 16
SCALE = 4
PIXEL_SIZE = 53
BASELINE = 12
FIRST = 0x20
LAST = 0x7E
COVERAGE = 0.38


def glyph(font: ImageFont.FreeTypeFont, code: int) -> list[int]:
    """Rasterize one codepoint at SCALE x, then area-sample to HEIGHT row bytes (MSB = leftmost)."""
    image = Image.new("L", (WIDTH * SCALE, HEIGHT * SCALE), 0)
    ImageDraw.Draw(image).text(
        (0, BASELINE * SCALE), chr(code), font=font, fill=255, anchor="ls"
    )
    rows = []
    for y in range(HEIGHT):
        row = 0
        for x in range(WIDTH):
            total = sum(
                image.getpixel((x * SCALE + dx, y * SCALE + dy))
                for dx in range(SCALE)
                for dy in range(SCALE)
            )
            if total / (255 * SCALE * SCALE) >= COVERAGE:
                row |= 0x80 >> x
        rows.append(row)
    return rows


def main() -> None:
    actual = hashlib.sha256(FACE.read_bytes()).hexdigest()
    if actual != FACE_SHA256:
        raise SystemExit(f"{FACE} sha256 {actual} != {FACE_SHA256}")
    font = ImageFont.truetype(str(FACE), PIXEL_SIZE)
    lines = [
        "//! 蓝屏使用的 8x16 ASCII 点阵字体（0x20..=0x7E，每字形 16 行，每行 MSB 为最左像素）。",
        "//!",
        "//! 由 `scripts/generate_crash_font.py` 从 JetBrains Mono NL Bold 生成；不要手工编辑。",
        "//! 内核早于文件系统存在，蓝屏又必须在任何服务都不可用时绘制，所以字形内嵌在内核镜像里。",
        "",
        "pub(super) const WIDTH: usize = 8;",
        "pub(super) const HEIGHT: usize = 16;",
        f"pub(super) const FIRST: u8 = {FIRST:#04x};",
        f"pub(super) const LAST: u8 = {LAST:#04x};",
        "",
        f"pub(super) static GLYPHS: [[u8; HEIGHT]; {LAST - FIRST + 1}] = [",
    ]
    for code in range(FIRST, LAST + 1):
        rows = ", ".join(f"{row:#04x}" for row in glyph(font, code))
        lines.append(f"    [{rows}], // {chr(code)!r}")
    lines.append("];")
    OUTPUT.write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
