#!/usr/bin/env python3
"""定位并调用 host e2fsprogs 的唯一共享 primitive。"""

from __future__ import annotations

import shutil
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
