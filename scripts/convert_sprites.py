#!/usr/bin/env python3
"""Pack the checked LiteOS desktop sprite sheet from the XP-style source PNGs.

把 `assets/sprites-src/` 下的源 PNG 按固定格子合成单张 ARGB 位图
`assets/desktop-sprites.argb`（桌面进程运行时从 rootfs 读入，不内嵌二进制）。
普通构建只消费合成产物；本脚本只在更换素材时手动运行
（需要 Pillow，使用 `target/fontenv` 虚拟环境）。

格子布局（与 `user/desktop/src/sprites.rs` 的常量一一对应）：
- (0,0)     start-normal   192x60   XP Start 按钮正常态
- (192,0)   start-hover    192x60   悬停态
- (384,0)   start-pressed  192x60   按下态
- (0,60)    avatar         84x84    开始菜单用户头像
- (84,60)   icon-terminal  60x60    终端图标（菜单左栏 30px@1×）
- (144,60)  icon-program   60x60    通用程序图标（菜单左栏 30px@1×）
- (204,60)  icon-power     44x44    关机图标（菜单底栏 22px@1×）
- (248,60)  icon-terminal  44x44    终端图标小档（菜单右栏 22px@1×，由 60px 缩小）
- (292,60)  icon-program   44x44    通用程序图标小档（同上）
"""

from __future__ import annotations

import hashlib
import struct
from pathlib import Path

try:
    from PIL import Image
except ModuleNotFoundError as error:
    raise SystemExit("convert-sprites requires Pillow; normal builds consume the checked asset") from error

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "assets/sprites-src"
OUTPUT = ROOT / "assets/desktop-sprites.argb"
MAGIC = b"LSP8\0\0\0\x01"
SHEET_WIDTH = 576
SHEET_HEIGHT = 144
# (文件名, 格子原点 x, y, 格子宽, 高)；尺寸与源 PNG 不一致时按格子 LANCZOS 缩小。
CELLS = (
    ("start-normal.png", 0, 0, 192, 60),
    ("start-hover.png", 192, 0, 192, 60),
    ("start-pressed.png", 384, 0, 192, 60),
    ("avatar.png", 0, 60, 84, 84),
    ("icon-terminal.png", 84, 60, 60, 60),
    ("icon-program.png", 144, 60, 60, 60),
    ("icon-power.png", 204, 60, 44, 44),
    ("icon-terminal.png", 248, 60, 44, 44),
)


def main() -> None:
    """Compose the fixed-grid ARGB sheet and print its identity."""
    sheet = Image.new("RGBA", (SHEET_WIDTH, SHEET_HEIGHT), (0, 0, 0, 0))
    for name, cell_x, cell_y, width, height in CELLS:
        image = Image.open(SOURCE / name).convert("RGBA")
        if image.size != (width, height):
            if image.size[0] < width or image.size[1] < height:
                raise RuntimeError(f"{name}: source {image.size} smaller than cell {width}x{height}")
            image = image.resize((width, height), Image.LANCZOS)
        sheet.paste(image, (cell_x, cell_y))
    rgba = sheet.tobytes()
    payload = bytearray(SHEET_WIDTH * SHEET_HEIGHT * 4)
    payload[0::4] = rgba[3::4]  # A
    payload[1::4] = rgba[0::4]  # R
    payload[2::4] = rgba[1::4]  # G
    payload[3::4] = rgba[2::4]  # B
    blob = MAGIC + struct.pack("<II", SHEET_WIDTH, SHEET_HEIGHT) + bytes(payload)
    temporary = OUTPUT.with_suffix(OUTPUT.suffix + ".tmp")
    temporary.write_bytes(blob)
    temporary.replace(OUTPUT)
    print(
        f"{OUTPUT.relative_to(ROOT)}: {SHEET_WIDTH}x{SHEET_HEIGHT}, {len(blob)} bytes, "
        f"sha256={hashlib.sha256(blob).hexdigest()}"
    )


if __name__ == "__main__":
    main()
