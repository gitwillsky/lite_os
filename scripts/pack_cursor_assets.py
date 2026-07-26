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
8B magic `LCR2\\0\\0\\0\\x01`、u32 width、u32 height，随后 width*height*4 字节
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
MAGIC = b"LCR2\0\0\0\x01"
# 光标物理边长。源图 64×64 高清绘制，按此尺寸抗锯齿降采样后打包，既保清晰
# 又控制视觉大小（48 物理 = 24 逻辑，介于经典 XP 的 16 与等大 32 之间）。
SIZE = 48

# 源 PNG 文件名 → 输出 .lc2 文件名（与 cursor.rs 的 6 个 CURSOR_* 形状对应）。
SHAPES = {
    "arrow": "cursor",
    "pointer": "cursor-pointer",
    "resize-ns": "cursor-resize-ns",
    "resize-ew": "cursor-resize-ew",
    "resize-nesw": "cursor-resize-nesw",
    "resize-nwse": "cursor-resize-nwse",
}


def pack(image: Image.Image) -> bytes:
    """把 64×64 RGBA 图像预乘并按 [B,G,R,A] 行主序打包为字节流。"""
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
    return MAGIC + struct.pack("<II", SIZE, SIZE) + bytes(payload)


def main() -> None:
    """把每个源 PNG 打包为对应的 `.lc2` 并打印 identity。"""
    for source_name, output_name in SHAPES.items():
        source = SOURCE / f"{source_name}.png"
        image = Image.open(source).convert("RGBA")
        # 源图为高清绘制（≥ SIZE）；抗锯齿降采样到目标物理尺寸，保清晰控大小。
        if image.size != (SIZE, SIZE):
            image = image.resize((SIZE, SIZE), Image.LANCZOS)
        blob = pack(image)
        output = ROOT / "assets" / f"{output_name}.lc2"
        output.write_bytes(blob)
        print(f"{output}: {SIZE}x{SIZE}, {len(blob)} bytes, sha256={hashlib.sha256(blob).hexdigest()}")


if __name__ == "__main__":
    main()
