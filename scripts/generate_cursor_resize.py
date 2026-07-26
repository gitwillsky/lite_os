#!/usr/bin/env python3
"""Generate the four checked resize cursor assets consumed by the compositor.

普通构建只消费 `assets/cursor-resize-*.lc1`；本脚本只在调整光标形状时手动
运行（需要 Pillow，优先使用 `target/fontenv` 虚拟环境）。

四个输出分别对应 CSS 的 `ns-resize`、`ew-resize`、`nesw-resize` 与
`nwse-resize`。每个形状都是以 (16,16) 为热点的经典双向箭头，使用白色填充
和黑色 1bpp 硬边。文件布局与 `generate_cursor.py` 完全相同。
"""

from __future__ import annotations

import hashlib
import math
import struct
from pathlib import Path

try:
    from PIL import Image, ImageChops, ImageDraw, ImageFilter
except ModuleNotFoundError as error:
    raise SystemExit(
        "generate-cursor-resize requires Pillow; normal builds consume the checked assets"
    ) from error

ROOT = Path(__file__).resolve().parents[1]
MAGIC = b"LCR1\0\0\0\x01"
SIZE = 32
SUPERSAMPLE = 16
BORDER = 1.3

# 以热点为中心的水平双向箭头。前两个坐标分别是轴向和法向距离；旋转该几何
# 得到另外三个方向，保证对边共享完全相同的尺寸和热点。
ARROW = [
    (-13.0, 0.0),
    (-7.0, -6.0),
    (-7.0, -2.0),
    (7.0, -2.0),
    (7.0, -6.0),
    (13.0, 0.0),
    (7.0, 6.0),
    (7.0, 2.0),
    (-7.0, 2.0),
    (-7.0, 6.0),
]

SHAPES = {
    "ns": 90.0,
    "ew": 0.0,
    "nesw": -45.0,
    "nwse": 45.0,
}


def polygon(angle: float, margin: int) -> list[tuple[float, float]]:
    """把标准双箭头旋转到指定物理方向并投影到超采样网格。"""
    radians = math.radians(angle)
    axis = (math.cos(radians), math.sin(radians))
    normal = (-axis[1], axis[0])
    center = 15.5 + margin
    return [
        (
            (center + along * axis[0] + across * normal[0]) * SUPERSAMPLE,
            (center + along * axis[1] + across * normal[1]) * SUPERSAMPLE,
        )
        for along, across in ARROW
    ]


def rasterize(angle: float) -> tuple[Image.Image, Image.Image]:
    """返回一个方向的 (黑色轮廓, 白色填充) 1bpp 掩膜。"""
    margin = 4
    grid = (SIZE + margin * 2) * SUPERSAMPLE
    outer = Image.new("L", (grid, grid), 0)
    ImageDraw.Draw(outer).polygon(polygon(angle, margin), fill=255)
    radius = int(round(BORDER * SUPERSAMPLE))
    inner = outer.filter(ImageFilter.MinFilter(radius * 2 + 1))
    ring = ImageChops.subtract(outer, inner)
    box = (
        margin * SUPERSAMPLE,
        margin * SUPERSAMPLE,
        (margin + SIZE) * SUPERSAMPLE,
        (margin + SIZE) * SUPERSAMPLE,
    )

    def shrink(mask: Image.Image) -> Image.Image:
        return Image.eval(
            mask.crop(box).resize((SIZE, SIZE), Image.LANCZOS),
            lambda value: 255 if value >= 128 else 0,
        ).convert("1")

    return shrink(ring), shrink(inner)


def pack_rows(mask: Image.Image) -> bytes:
    """把 1bpp 掩膜打包为 MSB-first 行主序字节流。"""
    rows = bytearray()
    for y in range(SIZE):
        row = 0
        for x in range(SIZE):
            if mask.getpixel((x, y)):
                row |= 0x80 >> (x & 7)
            if x & 7 == 7:
                rows.append(row)
                row = 0
    return bytes(rows)


def preview(name: str, outline: Image.Image, fill: Image.Image) -> Path:
    """写出一个 8× 棋盘背景预览图，供人工检查热点和方向。"""
    board = Image.new("RGB", (SIZE, SIZE))
    for y in range(SIZE):
        for x in range(SIZE):
            color = (200, 200, 200) if (x // 4 + y // 4) % 2 == 0 else (90, 140, 200)
            board.putpixel((x, y), color)
            if outline.getpixel((x, y)):
                board.putpixel((x, y), (0, 0, 0))
            elif fill.getpixel((x, y)):
                board.putpixel((x, y), (255, 255, 255))
    output = ROOT / f"target/cursor-resize-{name}-preview.png"
    output.parent.mkdir(parents=True, exist_ok=True)
    board.resize((SIZE * 8, SIZE * 8), Image.NEAREST).save(output)
    return output


def main() -> None:
    """生成全部四个方向，打印各自 identity 与预览路径。"""
    for name, angle in SHAPES.items():
        outline, fill = rasterize(angle)
        blob = MAGIC + struct.pack("<II", SIZE, SIZE) + pack_rows(outline) + pack_rows(fill)
        output = ROOT / f"assets/cursor-resize-{name}.lc1"
        output.write_bytes(blob)
        image = preview(name, outline, fill)
        print(
            f"{output}: {SIZE}x{SIZE}, {len(blob)} bytes, "
            f"sha256={hashlib.sha256(blob).hexdigest()}"
        )
        print(f"preview: {image}")


if __name__ == "__main__":
    main()
