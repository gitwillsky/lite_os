#!/usr/bin/env python3
"""Compose the checked LiteOS boot identity layers.

把 aurora logo、"LiteOS" 与启动状态分别生成最终物理像素尺寸的紧凑
premultiplied ARGB 图层。compositor 只负责在程序化背景上混合三层，不放大
低分辨率整屏画布。普通构建只消费 `assets/bootlogo.xrgb`；本脚本只在更换
批准的品牌资产时手动运行。
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
SOURCE = ROOT / "assets/splash/aurora-logo.png"
FONT = ROOT / "assets/fonts/liteos-ui-regular.otf"
OUTPUT = ROOT / "assets/bootlogo.xrgb"
MAGIC = b"LWP8\0\0\0\x03"
LOGO_HEIGHT = 530
TITLE = "LiteOS"
TITLE_SIZE = 192
STATUS = "Starting your workspace"
STATUS_SIZE = 34


def argb(image: Image.Image) -> bytes:
    """Return one RGBA image as little-endian premultiplied ARGB8888 bytes."""
    rgba = image.convert("RGBA").tobytes()
    payload = bytearray(image.width * image.height * 4)
    for offset in range(0, len(rgba), 4):
        red, green, blue, alpha = rgba[offset:offset + 4]
        pixel = offset
        payload[pixel] = (blue * alpha + 127) // 255
        payload[pixel + 1] = (green * alpha + 127) // 255
        payload[pixel + 2] = (red * alpha + 127) // 255
        payload[pixel + 3] = alpha
    return bytes(payload)


def even_canvas(image: Image.Image) -> Image.Image:
    """Pad a tight layer to even dimensions so its rectangle has an exact integer center."""
    width = image.width + image.width % 2
    height = image.height + image.height % 2
    canvas = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    canvas.paste(image, ((width - image.width) // 2, (height - image.height) // 2))
    return canvas


def logo_layer() -> Image.Image:
    """Crop transparent margins and downsample the approved mark once."""
    source = Image.open(SOURCE).convert("RGBA")
    bbox = source.getchannel("A").getbbox()
    if bbox is None:
        raise RuntimeError("boot logo source has no visible pixels")
    source = source.crop(bbox)
    width = round(source.width * LOGO_HEIGHT / source.height)
    return even_canvas(source.resize((width, LOGO_HEIGHT), Image.Resampling.LANCZOS))


def text_layer(text: str, size: int, color: tuple[int, int, int, int]) -> Image.Image:
    """Rasterize one tightly cropped text layer at final physical resolution."""
    font = ImageFont.truetype(str(FONT), size, layout_engine=ImageFont.Layout.BASIC)
    probe = Image.new("RGBA", (1, 1), (0, 0, 0, 0))
    bbox = ImageDraw.Draw(probe).textbbox((0, 0), text, font=font)
    layer = Image.new("RGBA", (bbox[2] - bbox[0], bbox[3] - bbox[1]), (0, 0, 0, 0))
    ImageDraw.Draw(layer).text((-bbox[0], -bbox[1]), text, font=font, fill=color)
    return even_canvas(layer)


def main() -> None:
    """Compose the three splash layers and write their checked binary payload."""
    logo = logo_layer()
    title = text_layer(TITLE, TITLE_SIZE, (248, 249, 255, 255))
    status = text_layer(STATUS, STATUS_SIZE, (196, 197, 209, 255))
    header = MAGIC + struct.pack(
        "<IIIIII",
        logo.width,
        logo.height,
        title.width,
        title.height,
        status.width,
        status.height,
    )
    blob = header + argb(logo) + argb(title) + argb(status)
    OUTPUT.write_bytes(blob)
    print(
        f"{OUTPUT}: logo={logo.width}x{logo.height}, title={title.width}x{title.height}, "
        f"status={status.width}x{status.height}, "
        f"{len(blob)} bytes, sha256={hashlib.sha256(blob).hexdigest()}"
    )


if __name__ == "__main__":
    main()
