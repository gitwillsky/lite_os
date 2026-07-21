#!/usr/bin/env python3
"""Generate the XP-style arrow cursor asset consumed by user/desktop.

普通构建只消费 `assets/cursor.lc1`；本脚本只在调整光标形状时手动运行
（需要 Pillow，使用 `target/fontenv` 虚拟环境）。

形状：经典 Windows 箭头（尖端在左上热点 (0,0)，白色填充 + 黑色轮廓，1bpp
硬边，与 XP 的 and/xor 掩膜光标一致，无抗锯齿）。绘制在 16× 超采样网格上，
轮廓由外形多边形均匀腐蚀得到填充区，再降采样阈值化，保证斜边落点平滑。
另输出 `target/cursor-preview.png`（8× 放大 + 棋盘背景）供人工检查。

文件布局（小端，与 user/desktop/src/cursor.rs 的解析契约一致）：
8B magic `LCR1\\0\\0\\0\\x01`、u32 width、u32 height，随后依次是轮廓与填充
两张 1bpp 位图，各 height*ceil(width/8) 字节，每字节 MSB 对应行内最左像素。
"""

from __future__ import annotations

import hashlib
import struct
from pathlib import Path

try:
    from PIL import Image, ImageChops, ImageDraw, ImageFilter
except ModuleNotFoundError as error:
    raise SystemExit("generate-cursor requires Pillow; normal builds consume the checked asset") from error

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "assets/cursor.lc1"
PREVIEW = ROOT / "target/cursor-preview.png"
MAGIC = b"LCR1\0\0\0\x01"
SIZE = 32
SUPERSAMPLE = 16

# 经典箭头外形多边形（32×32 网格，y 向下，尖端 (0,0)）：左竖边到底角，斜向
# 收回尾部凹口，再下探出尾刺，最后右翼回到斜边。整体高约 29px、宽约 18px，
# 与桌面 2× chrome 的物理尺寸匹配。
OUTLINE = [
    (0.0, 0.0),
    (0.0, 23.8),
    (6.3, 18.9),
    (10.5, 29.4),
    (14.0, 28.0),
    (9.8, 17.5),
    (17.5, 17.5),
]
# 轮廓厚度（px，32 网格）：XP 箭头约 1.2px 黑边。
BORDER = 1.3


def rasterize() -> tuple[Image.Image, Image.Image]:
    """返回 (轮廓掩膜, 填充掩膜)，均为 32×32 的 1bpp "1" 模式图。

    轮廓 = 外形 − 填充区（环形黑边）；绘制时加边距再裁剪，避免 PIL 滤波在
    图像边缘的像素复制破坏腐蚀结果。
    """
    margin = 4  # 32 网格像素
    grid = (SIZE + margin * 2) * SUPERSAMPLE
    outer = Image.new("L", (grid, grid), 0)
    draw = ImageDraw.Draw(outer)
    draw.polygon(
        [((x + margin) * SUPERSAMPLE, (y + margin) * SUPERSAMPLE) for x, y in OUTLINE],
        fill=255,
    )
    # 均匀腐蚀外形得到填充区：MinFilter 取邻域最小值，相当于二值腐蚀。
    radius = int(round(BORDER * SUPERSAMPLE))
    inner = outer.filter(ImageFilter.MinFilter(radius * 2 + 1))
    ring = ImageChops.subtract(outer, inner)
    box = (
        margin * SUPERSAMPLE,
        margin * SUPERSAMPLE,
        (margin + SIZE) * SUPERSAMPLE,
        (margin + SIZE) * SUPERSAMPLE,
    )
    shrink = lambda mask: Image.eval(
        mask.crop(box).resize((SIZE, SIZE), Image.LANCZOS), lambda v: 255 if v >= 128 else 0
    )
    return shrink(ring).convert("1"), shrink(inner).convert("1")


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


def preview(outline: Image.Image, fill: Image.Image) -> None:
    """在棋盘背景上叠加光标并 8× 放大，写出人工检查用预览图。"""
    board = Image.new("RGB", (SIZE, SIZE))
    for y in range(SIZE):
        for x in range(SIZE):
            board.putpixel((x, y), (200, 200, 200) if (x // 4 + y // 4) % 2 == 0 else (90, 140, 200))
    for y in range(SIZE):
        for x in range(SIZE):
            if outline.getpixel((x, y)):
                board.putpixel((x, y), (0, 0, 0))
            elif fill.getpixel((x, y)):
                board.putpixel((x, y), (255, 255, 255))
    PREVIEW.parent.mkdir(parents=True, exist_ok=True)
    board.resize((SIZE * 8, SIZE * 8), Image.NEAREST).save(PREVIEW)


def main() -> None:
    """生成光标资产并打印其 identity 与 ASCII 形状。"""
    outline, fill = rasterize()
    blob = MAGIC + struct.pack("<II", SIZE, SIZE) + pack_rows(outline) + pack_rows(fill)
    OUTPUT.write_bytes(blob)
    preview(outline, fill)
    for y in range(SIZE):
        print("".join("#" if outline.getpixel((x, y)) else "." if fill.getpixel((x, y)) else " " for x in range(SIZE)))
    print(f"{OUTPUT}: {SIZE}x{SIZE}, {len(blob)} bytes, sha256={hashlib.sha256(blob).hexdigest()}")
    print(f"preview: {PREVIEW}")


if __name__ == "__main__":
    main()
