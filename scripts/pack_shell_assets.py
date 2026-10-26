#!/usr/bin/env python3
"""将桌面 PNG 导出为标准 BITMAPV4HEADER 32 位 top-down BMP；普通构建只安装已提交产物。"""
import argparse
from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parents[1]
# 安装到系统界面目录的位图；原版独有的条目在仓库默认素材中缺失时，界面按缺图退化。
BITMAPS = {
    "logon-banner-1x", "start-banner-1x",
    "toolbar-default-1x", "toolbar-hot-1x", "toolbar-disabled-1x",
    "ie-toolbar-default-1x", "ie-toolbar-hot-1x", "ie-toolbar-disabled-1x",
    "go-default-1x", "go-hot-1x", "brand-22-1x", "brand-26-1x", "brand-38-1x",
    "webview-left-1x", "webview-line-1x", "worldmap-1x",
}


def pack(source: Path, destination: Path) -> None:
    from PIL import Image
    destination.mkdir(parents=True, exist_ok=True)
    for directory in ("icons-src", "bitmaps-src"):
        for path in sorted((source / directory).glob("*.png")):
            if directory == "icons-src" and not path.stem.endswith(("-16", "-32", "-64")):
                continue
            if directory == "bitmaps-src" and path.stem not in BITMAPS:
                continue
            image = Image.open(path).convert("RGBA")
            width, height = image.size
            pixels = image.tobytes("raw", "BGRA")
            header = struct.pack("<2sIHHI", b"BM", 122 + len(pixels), 0, 0, 122)
            header += struct.pack("<IiiHHIIiiII", 108, width, -height, 1, 32, 3, len(pixels), 0, 0, 0, 0)
            # V4 的显式 RGBA masks 与 sRGB，避免把 BI_RGB 的保留高字节私自解释为 alpha。
            header += struct.pack("<IIIII", 0x00ff0000, 0x0000ff00, 0x000000ff, 0xff000000, 0x73524742)
            header += bytes(48)
            (destination / (path.stem + ".bmp")).write_bytes(header + pixels)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--original", action="store_true")
    args = parser.parse_args()
    source = ROOT / (".local/inputs/windows2000/assets" if args.original else "assets/classic")
    destination = ROOT / (".local/inputs/windows2000/packed/ui" if args.original else "assets/classic/ui")
    pack(source, destination)
