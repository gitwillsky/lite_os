#!/usr/bin/env python3
"""Compose the checked boot splash asset from the source flag logo.

把 codex 生成的四色旗标（`assets/bootlogo-src.png`）与 "Microsoft Windows XP"
文字（host 侧 Noto Sans SC 渲染，splash 程序不含字体代码）合成到 1024x768
黑底画布，输出 raw XRGB。普通构建只消费 `assets/bootlogo.xrgb`；本脚本只在
更换源图时手动运行（需要 Pillow，使用 `target/fontenv` 虚拟环境）。
"""

from __future__ import annotations

import hashlib
import struct
from pathlib import Path

try:
    from PIL import Image, ImageDraw, ImageFont
except ModuleNotFoundError as error:
    raise SystemExit("convert-bootlogo requires Pillow; normal builds consume the checked asset") from error

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "assets/bootlogo-src.png"
FONT = ROOT / "target/font-cache/NotoSansCJKsc-Regular.otf"
OUTPUT = ROOT / "assets/bootlogo.xrgb"
MAGIC = b"LWP8\0\0\0\x01"
CANVAS = (1024, 768)
FLAG_HEIGHT = 360
FLAG_CENTER_Y = 0.42
TEXT = "Microsoft Windows XP"
TEXT_SIZE = 30
TEXT_Y = 0.62


def main() -> None:
    """Compose the splash canvas and write it as raw XRGB with the MAGIC header."""
    flag = Image.open(SOURCE).convert("RGB")
    # 源图是正方形、旗标居中：裁掉边缘黑边后按目标高度缩放。
    bbox = flag.getbbox()
    if bbox is not None:
        flag = flag.crop(bbox)
    ratio = FLAG_HEIGHT / flag.size[1]
    flag = flag.resize((round(flag.size[0] * ratio), FLAG_HEIGHT), Image.LANCZOS)

    canvas = Image.new("RGB", CANVAS, (0, 0, 0))
    canvas.paste(
        flag,
        (
            (CANVAS[0] - flag.size[0]) // 2,
            round(CANVAS[1] * FLAG_CENTER_Y - FLAG_HEIGHT / 2),
        ),
    )
    font = ImageFont.truetype(str(FONT), TEXT_SIZE, layout_engine=ImageFont.Layout.BASIC)
    draw = ImageDraw.Draw(canvas)
    width = draw.textlength(TEXT, font=font)
    draw.text(
        ((CANVAS[0] - width) / 2, CANVAS[1] * TEXT_Y),
        TEXT,
        font=font,
        fill=(255, 255, 255),
    )

    rgb = canvas.tobytes()
    payload = bytearray(CANVAS[0] * CANVAS[1] * 4)
    payload[0::4] = rgb[2::3]
    payload[1::4] = rgb[1::3]
    payload[2::4] = rgb[0::3]
    blob = MAGIC + struct.pack("<II", *CANVAS) + bytes(payload)
    OUTPUT.write_bytes(blob)
    print(f"{OUTPUT}: {CANVAS[0]}x{CANVAS[1]}, {len(blob)} bytes, sha256={hashlib.sha256(blob).hexdigest()}")


if __name__ == "__main__":
    main()
