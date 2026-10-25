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

from local_inputs import INPUTS

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


def pack(cursors: dict[str, dict[int, list]], hotspots: dict[str, tuple[int, int]] = HOTSPOTS) -> bytes:
    """编码光标集。

    Args:
        cursors: `名称 -> {尺寸: [帧图像...]}`；每个光标两种尺寸都必须有且帧数相同，图像必须是方形的对应尺寸。
        hotspots: `名称 -> 1x 热点`；2x 取其两倍。默认是仓库原创素材的热点。
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


# 原版 Windows 2000 默认方案（"_r" 常规白箭头系列）在安装介质 I386 里的文件名；热点取自 CUR 目录项。
# user32.dll 里导出的同名资源的热点字段不可信（全是 10,10），所以不用它们。
ORIGINAL_FILES = {
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

    count = struct.unpack_from("<HHH", data)[2]
    if struct.unpack_from("<HH", data, 2) != (2, 1) and count != 1:
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


def pack_original(sources: Path) -> bytes:
    """把安装介质的默认方案光标打包；原版只有 32 × 32，2x 是整数最近邻放大（热点同倍）。"""
    from PIL import Image

    cursors, hotspots = {}, {}
    for name, stem in ORIGINAL_FILES.items():
        image, hotspot = decode_cursor((sources / f"{stem}.cur").read_bytes())
        cursors[name] = {32: [image], 64: [image.resize((64, 64), Image.NEAREST)]}
        hotspots[name] = hotspot
    # 默认方案没有手形光标（它在 user32 里，热点字段不可信），所以 link 沿用仓库原创的手形。
    link = load(SOURCE, lambda path: Image.open(path))["link"]
    cursors["link"], hotspots["link"] = link, HOTSPOTS["link"]
    return pack(cursors, hotspots)


def main() -> None:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--original", action="store_true", help="打包本地原版 Windows 2000 默认方案到 .local/inputs/")
    args = parser.parse_args()
    try:
        from PIL import Image
    except ModuleNotFoundError as error:
        raise SystemExit("pack-cursors requires Pillow; normal builds consume the checked asset") from error
    if args.original:
        output = INPUTS / "windows2000/packed/cursors.lcs"
        output.parent.mkdir(parents=True, exist_ok=True)
        data = pack_original(INPUTS / "windows2000/assets/sources")
    else:
        output = OUTPUT
        data = pack(load(SOURCE, lambda path: Image.open(path)))
    output.write_bytes(data)
    print(f"{output.relative_to(ROOT)}: {len(data)} bytes", file=sys.stderr)


if __name__ == "__main__":
    main()
