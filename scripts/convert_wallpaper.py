#!/usr/bin/env python3
"""Convert the source wallpaper PNG into the checked LiteOS raw XRGB asset.

普通构建只消费 `assets/wallpaper.xrgb`；本脚本只在更换源图时手动运行
（需要 Pillow，使用 `target/fontenv` 虚拟环境）。
"""

from __future__ import annotations

import hashlib
import struct
from pathlib import Path

try:
    from PIL import Image
except ModuleNotFoundError as error:
    raise SystemExit("convert-wallpaper requires Pillow; normal builds consume the checked asset") from error

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "assets/wallpaper-src.png"
OUTPUT = ROOT / "assets/wallpaper.xrgb"
MAGIC = b"LWP8\0\0\0\x01"


def main() -> None:
    """Convert SOURCE PNG to raw XRGB8888 with the MAGIC header and print its identity."""
    image = Image.open(SOURCE).convert("RGB")
    width, height = image.size
    rgb = image.tobytes()
    payload = bytearray(width * height * 4)
    payload[0::4] = rgb[2::3]
    payload[1::4] = rgb[1::3]
    payload[2::4] = rgb[0::3]
    blob = MAGIC + struct.pack("<II", width, height) + bytes(payload)
    OUTPUT.write_bytes(blob)
    print(f"{OUTPUT}: {width}x{height}, {len(blob)} bytes, sha256={hashlib.sha256(blob).hexdigest()}")


if __name__ == "__main__":
    main()
