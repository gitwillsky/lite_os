"""经典桌面素材的来源选择：本地原版优先，缺失时用仓库提交的原创默认。

这是唯一的选择点。内核构建（编入启动画面）与 rootfs 构建（放入 compositor 读取的数据文件）都从这里取路径，
不各自判断来源，所以同一份构建不会出现内核用一份、compositor 用另一份的情况。
原版素材由 `scripts/extract_windows2000_assets.py` 提取、`scripts/pack_boot_scene.py --original` 打包，
只存在于被 Git 忽略的 `target/` 内。
"""

from __future__ import annotations

from pathlib import Path

from local_inputs import INPUTS

ROOT = Path(__file__).resolve().parents[1]
ORIGINAL_BOOT_SCENE = INPUTS / "windows2000/packed/boot-scene.lbs"
DEFAULT_BOOT_SCENE = ROOT / "assets/classic/boot-scene.lbs"
# 原版光标带 AND/XOR 掩码，需要 compositor 的软件光标路径后才能选用；在此之前只有仓库原创光标集。
DEFAULT_CURSORS = ROOT / "assets/classic/cursors.lcs"


def boot_scene() -> Path:
    """返回本次构建使用的 `LBS1` 启动画面：本地原版已打包则用它，否则用仓库默认。"""
    return ORIGINAL_BOOT_SCENE if ORIGINAL_BOOT_SCENE.is_file() else DEFAULT_BOOT_SCENE


def cursors() -> Path:
    """返回本次构建使用的 `LCS1` 光标集。"""
    return DEFAULT_CURSORS


def shell_bitmaps() -> tuple[Path, ...]:
    """按名称选择 BMP V4：显式打包的本地原版优先，缺失项使用已提交默认。"""
    original = INPUTS / "windows2000/packed/ui"
    return tuple(original / path.name if (original / path.name).is_file() else path
                 for path in sorted((ROOT / "assets/classic/ui").glob("*.bmp")))
