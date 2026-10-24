#!/usr/bin/env python3
"""把 `assets/classic/cursors-src/` 的光标 PNG 打包成 compositor 读取的 `LCS1` 光标集。

硬件光标上限 64×64，所以每个光标按 1x（32）与 2x（64）各存一份。帧数、帧间隔与热点随素材包提供，
compositor 不内置任何光标常量；换成别的素材包只需重新打包。像素是**预乘** alpha 的 `0xAARRGGBB`
（小端 u32），与 DRM `ARGB8888` 硬件光标直接一致。

格式（小端）：
    0   4  magic `LCS1`
    4   2  count   光标个数
    6   2  保留，必须为 0
    8   每个光标依次：
        16  name   NUL 补齐的 ASCII 名
        2   frames 帧数（>=1）
        2   frame_ms 帧间隔毫秒，单帧为 0
        4   hotspot 1x：x、y（u16）
        4   hotspot 2x：x、y（u16）
        4*32*32*frames   1x 各帧像素
        4*64*64*frames   2x 各帧像素

仓库内置的原创光标打包到 `assets/classic/cursors.lcs`（提交进库，普通构建不需要 Pillow）。
"""

from __future__ import annotations

import re
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "assets/classic/cursors-src"
OUTPUT = ROOT / "assets/classic/cursors.lcs"

MAGIC = b"LCS1"
SIZES = (32, 64)
# 仓库原创素材的 1x 热点（见 docs/design/classic-desktop/assets.md）；2x 为其两倍。
HOTSPOTS = {
    "arrow": (0, 0), "help": (0, 0), "working": (0, 0), "link": (6, 0),
    "text": (16, 16), "precision": (16, 16), "unavailable": (16, 16), "move": (16, 16), "busy": (16, 16),
    "resize-ns": (16, 16), "resize-ew": (16, 16), "resize-nwse": (16, 16), "resize-nesw": (16, 16),
}
# 多帧光标的帧间隔：原创沙漏 8 帧、10 fps。
FRAME_MS = 100
NAME_PATTERN = re.compile(r"^(?P<name>[a-z]+(?:-[a-z]+)*?)(?:-(?P<frame>\d+))?-(?P<size>32|64)\.png$")


def premultiplied(image) -> list[int]:
    """RGBA 直 alpha → 预乘的 `0xAARRGGBB`，四舍五入。"""
    raw = image.convert("RGBA").tobytes()
    pixels = []
    for offset in range(0, len(raw), 4):
        r, g, b, a = raw[offset:offset + 4]
        mul = lambda value: (value * a + 127) // 255
        pixels.append(a << 24 | mul(r) << 16 | mul(g) << 8 | mul(b))
    return pixels


def pack(cursors: dict[str, dict[int, list]]) -> bytes:
    """编码光标集。

    Args:
        cursors: `名称 -> {尺寸: [帧图像...]}`；每个光标两种尺寸都必须有且帧数相同，图像必须是方形的对应尺寸。
    """
    out = bytearray(MAGIC + struct.pack("<HH", len(cursors), 0))
    for name, by_size in sorted(cursors.items()):
        if name not in HOTSPOTS:
            raise ValueError(f"{name}: no hotspot is defined for this cursor")
        if set(by_size) != set(SIZES):
            raise ValueError(f"{name}: needs exactly the sizes {SIZES}")
        frames = len(by_size[32])
        if frames == 0 or len(by_size[64]) != frames:
            raise ValueError(f"{name}: 32 and 64 pixel frame counts differ")
        encoded = name.encode("ascii")
        if len(encoded) > 16:
            raise ValueError(f"{name}: name is longer than 16 bytes")
        hotspot = HOTSPOTS[name]
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


def load(directory: Path, open_image) -> dict[str, dict[int, list]]:
    """读取 `<名称>[-<帧>]-<尺寸>.png`，按帧号排序。"""
    frames: dict[str, dict[int, dict[int, object]]] = {}
    for path in sorted(directory.glob("*.png")):
        match = NAME_PATTERN.match(path.name)
        if not match:
            raise ValueError(f"unrecognised cursor file name: {path.name}")
        frame = int(match["frame"] or 0)
        frames.setdefault(match["name"], {}).setdefault(int(match["size"]), {})[frame] = open_image(path)
    cursors = {}
    for name, by_size in frames.items():
        cursors[name] = {}
        for size, by_frame in by_size.items():
            if sorted(by_frame) != list(range(len(by_frame))):
                raise ValueError(f"{name}: {size}px frames are not numbered contiguously from 0")
            cursors[name][size] = [by_frame[index] for index in range(len(by_frame))]
    return cursors


def main() -> None:
    try:
        from PIL import Image
    except ModuleNotFoundError as error:
        raise SystemExit("pack-cursors requires Pillow; normal builds consume the checked asset") from error
    data = pack(load(SOURCE, lambda path: Image.open(path)))
    OUTPUT.write_bytes(data)
    print(f"{OUTPUT.relative_to(ROOT)}: {len(data)} bytes", file=sys.stderr)


if __name__ == "__main__":
    main()
