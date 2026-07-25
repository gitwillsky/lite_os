#!/usr/bin/env python3
"""Compose the checked boot splash layers from the source flag logo.

把四色旗标与 "Microsoft Windows XP" 文字分别生成最终物理像素尺寸的紧凑
XRGB 图层。compositor 只负责把两层放到同一屏幕中轴，不再放大低分辨率整屏
画布。普通构建只消费 `assets/bootlogo.xrgb`；本脚本只在更换源图时手动运行。
"""

from __future__ import annotations

import hashlib
import struct
from pathlib import Path

try:
    from PIL import Image, ImageChops, ImageDraw, ImageFont
except ModuleNotFoundError as error:
    raise SystemExit("convert-bootlogo requires Pillow; normal builds consume the checked asset") from error

from generate_ui_font import REGULAR, REGULAR_SHA256, REGULAR_URL, ensure_font

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "assets/bootlogo-src.png"
OUTPUT = ROOT / "assets/bootlogo.xrgb"
MAGIC = b"LWP8\0\0\0\x02"
LOGO_HEIGHT = 360
VISIBLE_THRESHOLD = 3
TEXT = "Microsoft Windows XP"
TEXT_SIZE = 66


def xrgb(image: Image.Image) -> bytes:
    """Return one RGB image as little-endian XRGB8888 bytes."""
    rgb = image.tobytes()
    payload = bytearray(image.width * image.height * 4)
    payload[0::4] = rgb[2::3]
    payload[1::4] = rgb[1::3]
    payload[2::4] = rgb[0::3]
    return bytes(payload)


def even_canvas(image: Image.Image) -> Image.Image:
    """Pad a tight layer to even dimensions so its rectangle has an exact integer center."""
    width = image.width + image.width % 2
    height = image.height + image.height % 2
    canvas = Image.new("RGB", (width, height), (0, 0, 0))
    canvas.paste(image, ((width - image.width) // 2, (height - image.height) // 2))
    return canvas


def logo_layer() -> Image.Image:
    """Crop generated black margins and downsample the source once to its final size."""
    source = Image.open(SOURCE).convert("RGB")
    red, green, blue = source.split()
    visible = ImageChops.lighter(ImageChops.lighter(red, green), blue)
    bbox = visible.point(lambda value: 255 if value > VISIBLE_THRESHOLD else 0).getbbox()
    if bbox is None:
        raise RuntimeError("boot logo source has no visible pixels")
    source = source.crop(bbox)
    width = round(source.width * LOGO_HEIGHT / source.height)
    return even_canvas(source.resize((width, LOGO_HEIGHT), Image.Resampling.LANCZOS))


def title_layer() -> Image.Image:
    """Rasterize a tightly cropped title at the final 2× physical pixel size."""
    ensure_font(REGULAR, REGULAR_URL, REGULAR_SHA256)
    font = ImageFont.truetype(str(REGULAR), TEXT_SIZE, layout_engine=ImageFont.Layout.BASIC)
    probe = Image.new("RGB", (1, 1), (0, 0, 0))
    bbox = ImageDraw.Draw(probe).textbbox((0, 0), TEXT, font=font)
    title = Image.new("RGB", (bbox[2] - bbox[0], bbox[3] - bbox[1]), (0, 0, 0))
    ImageDraw.Draw(title).text((-bbox[0], -bbox[1]), TEXT, font=font, fill=(255, 255, 255))
    return even_canvas(title)


def main() -> None:
    """Compose the two splash layers and write their checked binary payload."""
    logo = logo_layer()
    title = title_layer()
    header = MAGIC + struct.pack("<IIII", logo.width, logo.height, title.width, title.height)
    blob = header + xrgb(logo) + xrgb(title)
    OUTPUT.write_bytes(blob)
    print(
        f"{OUTPUT}: logo={logo.width}x{logo.height}, title={title.width}x{title.height}, "
        f"{len(blob)} bytes, sha256={hashlib.sha256(blob).hexdigest()}"
    )


if __name__ == "__main__":
    main()
