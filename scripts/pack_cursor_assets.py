#!/usr/bin/env python3
"""把 Codex 生成的高清光标 PNG 打包成 compositor 消费的 `.lc2` 资产。

普通构建只消费 `assets/cursor*.lc2`；本脚本只在更换光标图形时手动运行
（需要 Pillow，优先使用 `target/fontenv` 虚拟环境）。它取代了旧的程序化
1bpp 绘制脚本（generate_cursor*.py），改由 `assets/cursors-src/*.png` 提供
真彩带 alpha 的图形。

输入：`assets/cursors-src/{arrow,pointer,resize-ns,resize-ew,resize-nesw,
resize-nwse}.png`，每个 64×64 RGBA。
输出：对应的 `assets/cursor{,-pointer,-resize-ns,...}.lc2`。

文件布局（小端，与 `user/compositor/src/cursor.rs` 的解析契约一致）：
8B magic `LCR2\\0\\0\\0\\x02`、u32 width、u32 height、u32 hot_x、u32 hot_y，随后
width*height*4 字节
**预乘** ARGB8888，行主序、上到下。每像素 4 字节按 `[B, G, R, A]` 顺序存放，
使 compositor 侧 `u32::from_le_bytes` 得到 `0xAARRGGBB`——正是 scanout 的
`over()` 期望的预乘形式（直 alpha 会让柔和边缘过亮）。
"""

from __future__ import annotations

import hashlib
import struct
from pathlib import Path

try:
    from PIL import Image
except ModuleNotFoundError as error:
    raise SystemExit(
        "pack-cursor-assets requires Pillow; normal builds consume the checked .lc2 assets"
    ) from error

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "assets/cursors-src"
MAGIC = b"LCR2\0\0\0\x02"
# 光标物理边长。源图 64×64 高清绘制，按此尺寸抗锯齿降采样后打包；
# 48 物理像素对应 HiDPI 下的 24 逻辑像素。
SIZE = 48

# 源 PNG 文件名 →（输出 .lc2 文件名，目标 48×48 物理像素热点）。热点是图形语义的一部分；
# 若只在 compositor 按 shape index 猜测，换图后指尖与 pointer position 会再次分离。
SHAPES = {
    "arrow": ("cursor", (0, 0)),
    "pointer": ("cursor-pointer", (18, 0)),
    "resize-ns": ("cursor-resize-ns", (24, 24)),
    "resize-ew": ("cursor-resize-ew", (24, 24)),
    "resize-nesw": ("cursor-resize-nesw", (24, 24)),
    "resize-nwse": ("cursor-resize-nwse", (24, 24)),
}


def pack(image: Image.Image, hotspot: tuple[int, int]) -> bytes:
    """把 RGBA 图像及其语义热点打包为 checked cursor asset。"""
    hot_x, hot_y = hotspot
    if not (0 <= hot_x < SIZE and 0 <= hot_y < SIZE):
        raise ValueError(f"cursor hotspot outside {SIZE}x{SIZE}: {hotspot!r}")
    payload = bytearray()
    for y in range(SIZE):
        for x in range(SIZE):
            r, g, b, a = image.getpixel((x, y))
            # 预乘：颜色通道按 alpha 缩放，over() 直接相加不再乘 alpha。
            # +127 后整除 255 做四舍五入，与 Rust 侧 over() 的舍入一致。
            pr = (r * a + 127) // 255
            pg = (g * a + 127) // 255
            pb = (b * a + 127) // 255
            payload.extend((pb, pg, pr, a))
    return MAGIC + struct.pack("<IIII", SIZE, SIZE, hot_x, hot_y) + bytes(payload)


def main() -> None:
    """把每个源 PNG 打包为对应的 `.lc2` 并打印 identity。"""
    for source_name, (output_name, hotspot) in SHAPES.items():
        source = SOURCE / f"{source_name}.png"
        image = Image.open(source).convert("RGBA")
        # 源图为高清绘制（≥ SIZE）；抗锯齿降采样到目标物理尺寸，保清晰控大小。
        if image.size != (SIZE, SIZE):
            image = image.resize((SIZE, SIZE), Image.LANCZOS)
        blob = pack(image, hotspot)
        output = ROOT / "assets" / f"{output_name}.lc2"
        output.write_bytes(blob)
        print(
            f"{output}: {SIZE}x{SIZE}, hotspot={hotspot}, {len(blob)} bytes, "
            f"sha256={hashlib.sha256(blob).hexdigest()}"
        )


if __name__ == "__main__":
    main()
