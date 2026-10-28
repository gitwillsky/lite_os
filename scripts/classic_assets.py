"""经典桌面产品资源清单：只消费 Git 中的 ISO 提取产物，缺失或损坏立即失败。"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DIRECTORY = ROOT / "assets/classic"


def check() -> dict:
    """校验受版本管理的产品资源及完整清单。

    Returns:
        包含 ISO 身份、每项来源和内容摘要的 manifest。

    Raises:
        OSError: manifest 或任何必需资源缺失、不可读取。
        ValueError: 路径越界、资源损坏或目录中有未登记的冗余文件。
    """
    manifest = json.loads((DIRECTORY / "manifest.json").read_text())
    for name, info in manifest["files"].items():
        path = (DIRECTORY / name).resolve()
        if not path.is_relative_to(DIRECTORY.resolve()):
            raise ValueError(f"asset path escapes product directory: {name}")
        data = path.read_bytes()
        if len(data) != info["bytes"] or hashlib.sha256(data).hexdigest() != info["sha256"]:
            raise ValueError(f"asset integrity mismatch: {name}")
    # Finder 的目录元数据不是产品资源；纳入清单会让浏览素材目录后的构建失败。
    actual = {str(path.relative_to(DIRECTORY)) for path in DIRECTORY.rglob("*")
              if path.is_file() and path.name != ".DS_Store"}
    expected = set(manifest["files"]) | {"manifest.json", "theme/classic.theme"}
    if actual != expected:
        raise ValueError(f"unregistered or missing product assets: {sorted(actual ^ expected)}")
    return manifest


def _files(group: str) -> tuple[Path, ...]:
    """返回已校验的安装组；组为空时报错，防止缺图或静音构建静默通过。"""
    paths = tuple(DIRECTORY / name for name, info in check()["files"].items() if info["group"] == group)
    if not paths:
        raise ValueError(f"empty product asset group: {group}")
    return paths


def boot_scene() -> Path:
    """返回 Git 中唯一的 LBS1 启动画面；资源缺失或损坏时失败。"""
    return _files("boot")[0]


def cursors() -> Path:
    """返回 Git 中唯一的 LCS1 默认光标包；资源缺失或损坏时失败。"""
    return _files("cursors")[0]


def sounds() -> tuple[tuple[str, Path], ...]:
    """返回必需提示音的 `(安装名, 源文件)`；资源缺失或损坏时失败。"""
    return tuple((path.name, path) for path in _files("sounds"))


def shell_bitmaps() -> tuple[Path, ...]:
    """返回必需的原生尺寸 BMP；不合并本地目录，不使用替代素材。"""
    return _files("ui")


def wallpapers() -> tuple[Path, ...]:
    """返回原版桌面墙纸 BMP，文件名即 Windows 2000 安装名；资源缺失或损坏时失败。"""
    return _files("wallpapers")


def fonts() -> tuple[Path, ...]:
    """返回原版字体与字体映射配置；资源缺失或损坏时失败。"""
    return _files("fonts")
