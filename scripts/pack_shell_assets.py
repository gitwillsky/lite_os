#!/usr/bin/env python3
"""将桌面 PNG 导出为标准 BITMAPV4HEADER top-down BMP（RGBA 或 8 位调色板）；普通构建只安装已提交产物。"""
from pathlib import Path
import struct


def pack(source: Path, destination: Path, names: set[str]) -> None:
    """转换导入器声明的 PNG，不发布未引用资源。

    Args:
        source: ISO 提取目录，包含 icons-src 与 bitmaps-src。
        destination: 本次发布的临时 ui 目录。
        names: 产品依赖的文件 stem，不含扩展名。

    Raises:
        OSError: 必需源图不可读或产物不可写。
        ValueError: 图像格式不可转换。
    """
    from PIL import Image
    destination.mkdir(parents=True, exist_ok=True)
    for directory in ("icons-src", "bitmaps-src"):
        for path in sorted((source / directory).glob("*.png")):
            if path.stem not in names:
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
