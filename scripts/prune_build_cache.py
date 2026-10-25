#!/usr/bin/env python3
"""预览或清理历史构建镜像；保留每个架构、每类最近两份缓存。"""

from __future__ import annotations

import argparse
from pathlib import Path

from build_cache import cache_lock, prune_apk_images, prune_rootfs_images
from build_target import SUPPORTED_ARCHITECTURES

ROOT = Path(__file__).resolve().parent.parent


def prune(*, apply: bool = False) -> int:
    """在各镜像 owner 的独占锁内预览或淘汰历史缓存。

    Args:
        apply: True 时删除预览项，缺省只打印路径与已分配空间。

    Returns:
        预览或删除的总字节数；按 inode 去重，不把稀疏镜像的逻辑长度当占用。

    Raises:
        OSError: 统计、锁或删除失败；with 释放锁，未处理条目可重试。
    """
    total = 0
    seen: set[tuple[int, int]] = set()
    for arch in SUPPORTED_ARCHITECTURES:
        for kind, lock, action in (
            ("busybox-runtime", ".rootfs.lock", prune_rootfs_images),
            ("apk-apps-runtime", ".installed.lock", prune_apk_images),
        ):
            work = ROOT / "target" / kind / arch
            if not work.is_dir():
                continue
            # 与 builder 共用锁，预览、统计和删除之间不会出现新的 cache writer。
            with cache_lock(work / lock):
                paths = action(work, dry_run=True)
                for path in paths:
                    size = 0
                    for file in path.rglob("*") if path.is_dir() else (path,):
                        if file.is_symlink() or not file.is_file():
                            continue
                        metadata = file.stat()
                        identity = (metadata.st_dev, metadata.st_ino)
                        if identity not in seen:
                            seen.add(identity)
                            size += metadata.st_blocks * 512
                    total += size
                    print(f"{'remove' if apply else 'preview'} {size / 1024**2:9.1f} MiB  {path.relative_to(ROOT)}")
                if apply:
                    action(work)
    print(f"{'Removed' if apply else 'Would remove'} {total / 1024**3:.2f} GiB of allocated cache files")
    return total


def main() -> int:
    """解析 --apply；直接运行脚本时默认仅预览。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="实际删除列出的历史镜像和 stamp")
    arguments = parser.parse_args()
    prune(apply=arguments.apply)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
