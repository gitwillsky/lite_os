#!/usr/bin/env python3
"""将 ISO 默认 CUR 方案编码成受版本管理的 LCS1 预乘 ARGB 光标包。

只在导入阶段转换：1x 保留原图，2x 最近邻放大，热点读取 CUR 目录。
普通构建与 compositor 只消费这一份光标包。

LCS1 小端布局：magic[4]、count u16、reserved u16；每项依次为 name[16]、frames u16、
frame_ms u16、1x/2x 热点各 (x u16, y u16)，随后为 32×32 和 64×64 的全部帧 ARGB u32 像素。
默认 CUR 方案只有静态帧，frame_ms 为零；不使用替代沙漏动画。
"""

from __future__ import annotations

import struct
from pathlib import Path


MAGIC = b"LCS1"
SIZES = (32, 64)
FRAME_MS = 100


def premultiplied(image) -> list[int]:
    """RGBA 直 alpha → 预乘的 `0xAARRGGBB`，四舍五入。"""
    raw = image.convert("RGBA").tobytes()
    pixels = []
    for offset in range(0, len(raw), 4):
        r, g, b, a = raw[offset:offset + 4]
        mul = lambda value: (value * a + 127) // 255
        pixels.append(a << 24 | mul(r) << 16 | mul(g) << 8 | mul(b))
    return pixels


def pack(cursors: dict[str, dict[int, list]], hotspots: dict[str, tuple[int, int]]) -> bytes:
    """编码光标集。

    Args:
        cursors: `名称 -> {尺寸: [帧图像...]}`；每个光标两种尺寸都必须有且帧数相同，图像必须是方形的对应尺寸。
        hotspots: `名称 -> 1x 热点`；2x 取其两倍。必须由原版 CUR 提供。
    """
    out = bytearray(MAGIC + struct.pack("<HH", len(cursors), 0))
    for name, by_size in sorted(cursors.items()):
        if name not in hotspots:
            raise ValueError(f"{name}: no hotspot is defined for this cursor")
        if set(by_size) != set(SIZES):
            raise ValueError(f"{name}: needs exactly the sizes {SIZES}")
        frames = len(by_size[32])
        if frames == 0 or len(by_size[64]) != frames:
            raise ValueError(f"{name}: 32 and 64 pixel frame counts differ")
        encoded = name.encode("ascii")
        if len(encoded) > 16:
            raise ValueError(f"{name}: name is longer than 16 bytes")
        hotspot = hotspots[name]
        out += encoded.ljust(16, b"\0") + struct.pack("<HH", frames, FRAME_MS if frames > 1 else 0)
        out += struct.pack("<HHHH", *hotspot, hotspot[0] * 2, hotspot[1] * 2)
        for size in SIZES:
            for image in by_size[size]:
                if image.size != (size, size):
                    raise ValueError(f"{name}: a {size}px frame is {image.size[0]}x{image.size[1]}")
                out += b"".join(struct.pack("<I", pixel) for pixel in premultiplied(image))
    return bytes(out)


def unpack(data: bytes) -> dict[str, dict]:
    """参考解码器：返回 `名称 -> {frames, frame_ms, hotspot: {32, 64}, pixels: {32, 64}}`，供测试与人工核对。"""
    if data[:4] != MAGIC:
        raise ValueError("bad magic")
    count, reserved = struct.unpack_from("<HH", data, 4)
    if reserved:
        raise ValueError("bad header")
    offset, result = 8, {}
    for _ in range(count):
        name = data[offset:offset + 16].rstrip(b"\0").decode("ascii")
        frames, frame_ms, x1, y1, x2, y2 = struct.unpack_from("<HHHHHH", data, offset + 16)
        offset += 28
        pixels = {}
        for size in SIZES:
            length = size * size * frames
            pixels[size] = list(struct.unpack_from(f"<{length}I", data, offset))
            offset += 4 * length
        result[name] = {"frames": frames, "frame_ms": frame_ms, "hotspot": {32: (x1, y1), 64: (x2, y2)}, "pixels": pixels}
    if offset != len(data):
        raise ValueError("trailing bytes")
    return result



# 原版 Windows 2000 默认方案（"_r" 常规白箭头系列）在安装介质 I386 里的文件名；热点取自 CUR 目录项。
# 安装文件的默认方案与 user32.dll 的同名资源不同，不能拿 DLL 的热点覆盖默认方案。
CURSOR_FILES = {
    "arrow": "arrow_r", "text": "beam_r", "busy": "busy_r", "working": "wait_r", "precision": "cross_r",
    "unavailable": "no_r", "move": "move_r", "help": "help_r",
    "resize-nesw": "size1_r", "resize-nwse": "size2_r", "resize-ew": "size3_r", "resize-ns": "size4_r",
}


def decode_cursor(data: bytes):
    """解码单图像 1/4/8 位 CUR，返回 `(32×32 RGBA 图像, (热点 x, 热点 y))`。

    AND=0 取 XOR 颜色且不透明；AND=1 且 XOR 为黑是透明；AND=1 且 XOR 非黑是"反色"，
    硬件光标无法表示，遇到即报错（默认方案的光标没有这种像素）。
    """
    from PIL import Image

    if struct.unpack_from("<HHH", data) != (0, 2, 1):
        raise ValueError("not a single-image cursor")
    width, height, _, _, hot_x, hot_y, _, offset = struct.unpack_from("<BBBBHHII", data, 6)
    width, height = width or 256, height or 256
    header, _, _, _, bpp, _, _, _, _, used, _ = struct.unpack_from("<IiiHHIIiiII", data, offset)
    if bpp not in (1, 4, 8) or (width, height) != (32, 32):
        raise ValueError(f"unsupported cursor: {width}x{height} {bpp}bpp")
    entries = used or 1 << bpp
    palette = [struct.unpack_from("<BBB", data, offset + header + 4 * i) for i in range(entries)]
    base = offset + header + 4 * entries
    xor_stride, and_stride = (width * bpp + 31) // 32 * 4, (width + 31) // 32 * 4
    and_base = base + xor_stride * height
    pixels = bytearray()
    for y in range(height):
        row = height - 1 - y
        for x in range(width):
            if bpp == 1:
                index = data[base + row * xor_stride + x // 8] >> (7 - x % 8) & 1
            elif bpp == 4:
                byte = data[base + row * xor_stride + x // 2]
                index = byte >> 4 if x % 2 == 0 else byte & 15
            else:
                index = data[base + row * xor_stride + x]
            blue, green, red = palette[index]
            masked = data[and_base + row * and_stride + x // 8] >> (7 - x % 8) & 1
            if masked and (red, green, blue) != (0, 0, 0):
                raise ValueError("cursor uses inverted pixels")
            pixels += bytes((0, 0, 0, 0) if masked else (red, green, blue, 255))
    return Image.frombytes("RGBA", (width, height), bytes(pixels)), (hot_x, hot_y)


def pack_sources(sources: Path, link: Path) -> bytes:
    """打包安装介质的默认方案，2x 最近邻放大并同步缩放热点。

    Args:
        sources: ISO 安装文件解压目录，包含默认方案的 CUR 文件。
        link: user32.dll:114 导出的原始手形 CUR。

    Returns:
        可由 compositor 直接解码的 LCS1 字节。

    Raises:
        OSError: 原版 CUR 缺失或不可读取。
        ValueError: CUR 格式不支持，或 AND/XOR 含硬件光标不能表达的反色像素。
    """
    from PIL import Image

    cursors, hotspots = {}, {}
    for name, stem in CURSOR_FILES.items():
        image, hotspot = decode_cursor((sources / f"{stem}.cur").read_bytes())
        cursors[name] = {32: [image], 64: [image.resize((64, 64), Image.NEAREST)]}
        hotspots[name] = hotspot
    # 手形来自 user32.dll:114；沿用原始 CUR 热点，不再混入原创光标。
    image, hotspot = decode_cursor(link.read_bytes())
    cursors["link"] = {32: [image], 64: [image.resize((64, 64), Image.NEAREST)]}
    hotspots["link"] = hotspot
    return pack(cursors, hotspots)
