#!/usr/bin/env python3
"""经典界面资源转换：导入 PNG 为 BMP V4，普通构建从已提交 BMP 生成只读图集。"""
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


def atlas(paths: tuple[Path, ...], destination: Path) -> None:
    """把唯一导入器产出的 BMP V4 预解码为 LUIA0001，只读映射时无需复制像素。

    Args:
        paths: 已由 classic_assets 校验的全部 UI 输入；只接受导入器的 8-bit/32-bit top-down 编码。
        destination: 原子发布目录中的最终 atlas 文件；调用者负责缓存与安装。

    Raises:
        OSError: 输入不可读或输出不可写。
        ValueError: 输入不是导入器约定的格式、尺寸越界或名称重复。
    """
    import re
    images = {}
    for path in paths:
        data = path.read_bytes()
        if len(data) < 122 or data[:2] != b"BM":
            raise ValueError(f"not an imported BMP: {path}")
        offset = struct.unpack_from("<I", data, 10)[0]
        header, width, height, planes, bits, compression = struct.unpack_from("<IiiHHI", data, 14)
        if header != 108 or not 0 < width <= 2048 or not -2048 <= height < 0 or planes != 1:
            raise ValueError(f"unexpected imported BMP header: {path}")
        height = -height
        indices = b""
        if bits == 32 and compression == 3:
            masks = struct.unpack_from("<IIII", data, 54)
            if masks != (0x00ff0000, 0x0000ff00, 0x000000ff, 0xff000000) or offset != 122:
                raise ValueError(f"unexpected imported BMP masks: {path}")
            source = data[offset:]
            if len(source) != width * height * 4:
                raise ValueError(f"truncated imported BMP: {path}")
            pixels = bytearray(len(source))
            for start in range(0, len(source), 4):
                blue, green, red, alpha = source[start:start + 4]
                pixels[start:start + 4] = bytes((blue * alpha // 255, green * alpha // 255,
                                               red * alpha // 255, alpha))
        elif bits == 8 and compression == 0 and offset == 122 + 1024:
            stride = (width + 3) // 4 * 4
            if len(data) != offset + stride * height:
                raise ValueError(f"truncated imported indexed BMP: {path}")
            palette = [data[start:start + 3] + b"\xff" for start in range(122, offset, 4)]
            indices = b"".join(data[offset + y * stride:offset + y * stride + width] for y in range(height))
            pixels = b"".join(palette[index] for index in indices)
        else:
            raise ValueError(f"unsupported imported BMP encoding: {path}")
        if path.name in images:
            raise ValueError(f"duplicate atlas image: {path.name}")
        images[path.name] = (width, height, bytes(pixels), indices)

    # 1. 在构建时完成 load_icon 的同尺寸/较大/较小选择和最近邻缩放；运行时不再为缺尺寸分配私有像素。
    native = images.copy()
    stems = {match[1] for name in native if (match := re.fullmatch(r"(.+)-(16|32|64)\.bmp", name))}
    for stem in sorted(stems):
        for size, candidates in ((16, (16, 32, 64)), (32, (32, 64, 16)), (64, (64, 32, 16))):
            name = f"{stem}-{size}.bmp"
            if name in images:
                continue
            width, height, pixels, indices = next(native[f"{stem}-{candidate}.bmp"]
                                                  for candidate in candidates if f"{stem}-{candidate}.bmp" in native)
            positions = [(y * height // size) * width + x * width // size
                         for y in range(size) for x in range(size)]
            images[name] = (size, size, b"".join(pixels[position * 4:position * 4 + 4] for position in positions),
                            bytes(indices[position] for position in positions) if indices else b"")

    # 2. 固定头与按名排序的固定记录可直接二分查找；像素按 u32 对齐，下标原样保留供重新着色。
    count = len(images)
    if count == 0:
        raise ValueError("empty bitmap atlas")
    data = bytearray((32 + count * 128 + 4095) // 4096 * 4096)
    for index, (name, (width, height, pixels, indices)) in enumerate(sorted(images.items())):
        encoded = name.encode("ascii")
        if not encoded or len(encoded) >= 96 or not re.fullmatch(rb"[A-Za-z0-9_-]+\.bmp", encoded):
            raise ValueError(f"invalid atlas name: {name}")
        start = 32 + index * 128
        data[start:start + len(encoded)] = encoded
        pixel_offset = len(data)
        data.extend(pixels)
        index_offset = len(data) if indices else 0
        data.extend(indices)
        data.extend(bytes((-len(data)) % 4))
        struct.pack_into("<IIQQQ", data, start + 96, width, height, pixel_offset, index_offset, 0)
    struct.pack_into("<8sIIQQ", data, 0, b"LUIA0001", count, 128, len(data), 0)
    destination.write_bytes(data)
