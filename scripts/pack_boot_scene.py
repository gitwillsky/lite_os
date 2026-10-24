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

仓库内置的原创默认画面打包到 `assets/classic/boot-scene.lbs`（提交进仓库，普通构建不需要 Pillow）；
`--original` 把本地提取的原版画面打包到被 Git 忽略的 `.local/inputs/` 内，由 `scripts/classic_assets.py` 优先选用。
"""

from __future__ import annotations

import argparse
import struct
import sys
from pathlib import Path

from local_inputs import INPUTS

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SOURCE = ROOT / "assets/classic/bitmaps-src/boot-scene-1x.png"
DEFAULT_OUTPUT = ROOT / "assets/classic/boot-scene.lbs"
ORIGINAL_SOURCE = INPUTS / "windows2000/assets/bitmaps-src/boot-scene-1x.png"
ORIGINAL_OUTPUT = INPUTS / "windows2000/packed/boot-scene.lbs"

MAGIC = b"LBS1"
WIDTH, HEIGHT = 640, 480
# 进度槽外框 x 273..437、y 436..445（1 像素凹边），填充只画内部；位置与颜色取自原版启动画面的实测值。
FILL_RECT = (274, 437, 163, 8)
FILL_COLOR = 0x00299C


# 降色时必须逐值保留的颜色：进度槽凹边、底栏、分隔带与背景；它们的位置由 FILL_RECT 与版式契约决定，
# 并入最近色会让槽边框或底栏出现色差。
EXACT_COLORS = ((0, 0, 0), (255, 255, 255), (192, 192, 192), (128, 128, 128), (0, 41, 156))


def reduce_colors(pixels: list[tuple[int, int, int]], limit: int = 256) -> list[tuple[int, int, int]]:
    """把颜色数降到 limit：EXACT_COLORS 先占位，其余按像素数从多到少保留，其余并入最近色（无抖动）。"""
    counts: dict[tuple[int, int, int], int] = {}
    for pixel in pixels:
        counts[pixel] = counts.get(pixel, 0) + 1
    if len(counts) <= limit:
        return pixels
    kept = [c for c in EXACT_COLORS if c in counts]
    kept += [c for c, _ in sorted(counts.items(), key=lambda item: -item[1]) if c not in kept][: limit - len(kept)]
    nearest = {
        color: color if color in kept else min(kept, key=lambda k: sum((a - b) ** 2 for a, b in zip(color, k)))
        for color in counts
    }
    print(f"reduce: {len(counts)} colors -> {len(kept)}", file=sys.stderr)
    return [nearest[pixel] for pixel in pixels]


def pack(image, reduce: bool = False) -> bytes:
    """把 640×480 的 RGB 图编码成 LBS1。尺寸不符报错；超过 256 色时只有显式 reduce 才降色，否则报错。"""
    if image.size != (WIDTH, HEIGHT):
        raise ValueError(f"boot scene must be {WIDTH}x{HEIGHT}, got {image.size[0]}x{image.size[1]}")
    raw = image.convert("RGB").tobytes()
    pixels = [tuple(raw[i:i + 3]) for i in range(0, len(raw), 3)]
    if reduce:
        pixels = reduce_colors(pixels)
    palette: dict[tuple[int, int, int], int] = {}
    for pixel in pixels:
        palette.setdefault(pixel, len(palette))
    if len(palette) > 256:
        raise ValueError(f"boot scene uses {len(palette)} colors; the limit is 256 (pass --reduce to merge)")
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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--original", action="store_true", help="打包本地提取的原版画面到 .local/inputs/")
    parser.add_argument("--reduce", action="store_true", help="颜色超过 256 时显式合并为最近色（会打印合并数量）")
    args = parser.parse_args()
    source, output = (ORIGINAL_SOURCE, ORIGINAL_OUTPUT) if args.original else (DEFAULT_SOURCE, DEFAULT_OUTPUT)
    try:
        from PIL import Image
    except ModuleNotFoundError as error:
        raise SystemExit("pack-boot-scene requires Pillow; normal builds consume the checked asset") from error
    if not source.exists():
        raise SystemExit(f"missing source: {source}")
    with Image.open(source) as image:
        data = pack(image, reduce=args.reduce)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(data)
    print(f"{output.relative_to(ROOT)}: {len(data)} bytes", file=sys.stderr)


if __name__ == "__main__":
    main()
