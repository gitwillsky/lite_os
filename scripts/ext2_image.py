#!/usr/bin/env python3
"""定位并调用 host e2fsprogs 的唯一共享 primitive。"""

from __future__ import annotations

import shutil
import struct
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def find_mke2fs() -> Path:
    """返回可执行 mke2fs；PATH 与常见 Homebrew/system 路径均不存在时 fail-stop。"""
    candidates = (
        shutil.which("mke2fs"),
        "/opt/homebrew/opt/e2fsprogs/sbin/mke2fs",
        "/usr/local/opt/e2fsprogs/sbin/mke2fs",
        "/usr/sbin/mke2fs",
    )
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return Path(candidate)
    raise RuntimeError("mke2fs from e2fsprogs is required")


def find_debugfs() -> Path:
    """返回可执行 debugfs；PATH 与常见 Homebrew/system 路径均不存在时 fail-stop。"""
    candidates = (
        shutil.which("debugfs"),
        "/opt/homebrew/opt/e2fsprogs/sbin/debugfs",
        "/usr/local/opt/e2fsprogs/sbin/debugfs",
        "/usr/sbin/debugfs",
    )
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return Path(candidate)
    raise RuntimeError("debugfs from e2fsprogs is required")


def find_e2fsck() -> Path:
    """返回可执行 e2fsck；PATH 与常见 Homebrew/system 路径均不存在时 fail-stop。"""
    candidates = (
        shutil.which("e2fsck"),
        "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck",
        "/usr/local/opt/e2fsprogs/sbin/e2fsck",
        "/usr/sbin/e2fsck",
    )
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return Path(candidate)
    raise RuntimeError("e2fsck from e2fsprogs is required")


def find_resize2fs() -> Path:
    """返回可执行 resize2fs；PATH 与常见 Homebrew/system 路径均不存在时 fail-stop。"""
    candidates = (
        shutil.which("resize2fs"),
        "/opt/homebrew/opt/e2fsprogs/sbin/resize2fs",
        "/usr/local/opt/e2fsprogs/sbin/resize2fs",
        "/usr/sbin/resize2fs",
    )
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return Path(candidate)
    raise RuntimeError("resize2fs from e2fsprogs is required")


def ext2_capacity_bytes(image: Path) -> int:
    """返回 ext2/3/4 超级块声明的文件系统容量；格式或元数据非法时 fail-stop。"""
    with image.open("rb") as stream:
        stream.seek(1024)
        superblock = stream.read(1024)
    if len(superblock) != 1024 or struct.unpack_from("<H", superblock, 56)[0] != 0xEF53:
        raise RuntimeError(f"not an ext2/3/4 filesystem image: {image}")
    log_block_size = struct.unpack_from("<I", superblock, 24)[0]
    if log_block_size > 6:
        raise RuntimeError(f"invalid ext filesystem block size in {image}")
    blocks = struct.unpack_from("<I", superblock, 4)[0]
    incompat_features = struct.unpack_from("<I", superblock, 96)[0]
    if incompat_features & 0x80:
        blocks |= struct.unpack_from("<I", superblock, 336)[0] << 32
    return blocks * (1024 << log_block_size)


def ensure_ext2_capacity(image: Path, size_mib: int) -> None:
    """把离线 ext 镜像扩到至少 size_mib，保留已有文件且绝不执行缩容。"""
    if size_mib <= 0:
        raise ValueError("ext filesystem capacity must be positive")
    requested_bytes = size_mib * 1024 * 1024
    current_bytes = ext2_capacity_bytes(image)
    if current_bytes >= requested_bytes:
        return

    with image.open("r+b") as stream:
        stream.truncate(requested_bytes)

    # guest 被窗口关闭时 journal 可能仍需 replay；只恢复 journal，避免 host fsck 对
    # LiteOS 已接受的目录项或 symlink 施加额外修复策略。
    check = subprocess.run(
        [str(find_e2fsck()), "-E", "journal_only", "-p", str(image)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if check.returncode not in (0, 1):
        tail = "\n".join(check.stdout.splitlines()[-40:])
        raise RuntimeError(f"journal recovery failed for {image}\n{tail}")

    resize = subprocess.run(
        [str(find_resize2fs()), str(image)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if resize.returncode != 0:
        tail = "\n".join(resize.stdout.splitlines()[-40:])
        raise RuntimeError(f"ext filesystem resize failed for {image}\n{tail}")
    actual_bytes = ext2_capacity_bytes(image)
    if actual_bytes < requested_bytes:
        raise RuntimeError(
            f"ext filesystem resize was incomplete: {actual_bytes} < {requested_bytes}"
        )


def run_debugfs(image: Path, request: str, *, writable: bool = False) -> str:
    """执行一次 debugfs request，并把 stdout/stderr 统一为可诊断异常。"""
    command = [str(find_debugfs())]
    if writable:
        command.append("-w")
    command.extend(("-R", request, str(image)))
    result = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-40:])
        raise RuntimeError(f"debugfs request failed: {request}\n{tail}")
    return result.stdout
