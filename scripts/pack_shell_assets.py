#!/usr/bin/env python3
"""将桌面 PNG 导出为标准 BITMAPV4HEADER top-down BMP（RGBA 或 8 位调色板）；普通构建只安装已提交产物。"""
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
            with Image.open(path) as source_image:
                width, height = source_image.size
                indexed = source_image.mode == "P" and "transparency" not in source_image.info
                palette = b""
                if indexed:
                    # RGBQUAD 第四字节是保留位，必须为零；每行补齐四字节，不改变区域下标。
                    colors = source_image.getpalette("RGB")
                    colors = colors + [0] * (768 - len(colors))
                    palette = b"".join(bytes((colors[i + 2], colors[i + 1], colors[i], 0))
                                       for i in range(0, 768, 3))
                    indices = source_image.tobytes()
                    padding = bytes((-width) % 4)
                    pixels = b"".join(indices[y * width:(y + 1) * width] + padding for y in range(height))
                    bits, compression, masks, used = 8, 0, (0, 0, 0, 0), 256
                else:
                    pixels = source_image.convert("RGBA").tobytes("raw", "BGRA")
                    bits, compression = 32, 3
                    masks, used = (0x00ff0000, 0x0000ff00, 0x000000ff, 0xff000000), 0
                offset = 122 + len(palette)
                header = struct.pack("<2sIHHI", b"BM", offset + len(pixels), 0, 0, offset)
                header += struct.pack("<IiiHHIIiiII", 108, width, -height, 1, bits,
                                      compression, len(pixels), 0, 0, used, 0)
                header += struct.pack("<IIIII", *masks, 0x73524742)
                header += bytes(48)
                (destination / (path.stem + ".bmp")).write_bytes(header + palette + pixels)



if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--original", action="store_true")
    args = parser.parse_args()
    source = ROOT / (".local/inputs/windows2000/assets" if args.original else "assets/classic")
    destination = ROOT / (".local/inputs/windows2000/packed/ui" if args.original else "assets/classic/ui")
    pack(source, destination)
