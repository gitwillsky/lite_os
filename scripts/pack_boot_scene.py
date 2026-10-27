#!/usr/bin/env python3
"""把 640×480 启动画面 PNG 打包成内核与 compositor 共用的 `LBS1` 紧凑位图。

内核在文件系统挂载之前就要显示启动画面，所以位图随内核编入；compositor 从 rootfs 的数据文件读取同一格式。
两端只解码这一种格式，进度槽的位置与填充色由文件头给出，不在代码里各存一份。

格式（小端）：
    0   4  magic `LBS1`
    4   2  width            （固定 640）
    6   2  height           （固定 480）
    8   2  palette_len      （1..=256）
    10  2  保留，必须为 0
    12  8  进度槽内部填充矩形 x、y、w、h（各 u16）
    20  4  填充色 0x00RRGGBB
    24  4*palette_len  调色板，每项 0x00RRGGBB
    ..  RLE：(run u8 ∈ 1..=255, index u8) 对，行主序、上到下，像素总数恰为 width*height，文件到此结束

资源只由 ISO 导入器生成到 `assets/classic/boot-scene.lbs`，普通构建直接消费受版本管理的产物。
"""

from __future__ import annotations

import struct

MAGIC = b"LBS1"
WIDTH, HEIGHT = 640, 480
# 进度槽外框 x 273..437、y 436..445（1 像素凹边），填充只画内部；位置与颜色取自原版启动画面的实测值。
FILL_RECT = (274, 437, 163, 8)
FILL_COLOR = 0x00299C


def pack(image) -> bytes:
    """把 640×480 的 RGB 图编码成 LBS1。尺寸不符报错；超过 256 色时报错，不更改原版像素。"""
    if image.size != (WIDTH, HEIGHT):
        raise ValueError(f"boot scene must be {WIDTH}x{HEIGHT}, got {image.size[0]}x{image.size[1]}")
    raw = image.convert("RGB").tobytes()
    pixels = [tuple(raw[i:i + 3]) for i in range(0, len(raw), 3)]
    palette: dict[tuple[int, int, int], int] = {}
    for pixel in pixels:
        palette.setdefault(pixel, len(palette))
    if len(palette) > 256:
        raise ValueError(f"boot scene uses {len(palette)} colors; the limit is 256")
    payload = bytearray()
    run, current = 0, None
    for pixel in pixels:
        index = palette[pixel]
        if index == current and run < 255:
            run += 1
            continue
        if run:
            payload += bytes((run, current))
        run, current = 1, index
    payload += bytes((run, current))
    header = MAGIC + struct.pack("<HHHH", WIDTH, HEIGHT, len(palette), 0)
    header += struct.pack("<HHHH", *FILL_RECT) + struct.pack("<I", FILL_COLOR)
    colors = b"".join(struct.pack("<I", r << 16 | g << 8 | b) for (r, g, b) in palette)
    return header + colors + bytes(payload)


def unpack(data: bytes) -> tuple[int, int, list[tuple[int, int, int]]]:
    """参考解码器：与内核/compositor 的校验规则一致，供测试与人工核对使用。"""
    if data[:4] != MAGIC:
        raise ValueError("bad magic")
    width, height, count, reserved = struct.unpack_from("<HHHH", data, 4)
    if (width, height) != (WIDTH, HEIGHT) or not 1 <= count <= 256 or reserved:
        raise ValueError("bad header")
    start = 24 + 4 * count
    palette = [struct.unpack_from("<I", data, 24 + 4 * i)[0] for i in range(count)]
    colors = [(c >> 16 & 255, c >> 8 & 255, c & 255) for c in palette]
    body = data[start:]
    if len(body) % 2:
        raise ValueError("odd RLE length")
    out: list[tuple[int, int, int]] = []
    for run, index in zip(body[0::2], body[1::2]):
        if run == 0 or index >= count:
            raise ValueError("bad RLE pair")
        out += [colors[index]] * run
    if len(out) != width * height:
        raise ValueError("pixel count mismatch")
    return width, height, out
