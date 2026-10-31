#!/usr/bin/env python3
"""在 Apple Silicon macOS 上一次性准备 LiteOS host 构建、运行与门禁工具。

本脚本只安装 workflow 已经消费的外部工具，不改变任何构建输入的解析规则：
1. 校验 macOS/arm64 与 Xcode Command Line Tools；
2. 用 Homebrew 安装 Clang/LLVM archive 工具、e2fsprogs、QEMU 与 OpenSSL；
3. 安装 rustup 并按 ``rust-toolchain.toml`` 安装固定 nightly、组件与 target；
4. 从固定输入构建带 xHCI 修复的 UTM 和 USB 转发程序；

所有步骤幂等；已满足的步骤直接跳过。
"""

from __future__ import annotations

import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

from local_inputs import INPUTS
from typing import Sequence

from utm_build import ensure_built

ROOT = Path(__file__).resolve().parent.parent
DOWNLOADS = INPUTS / "host"
# llvm 与 e2fsprogs 为 keg-only；verify_musl/ext4_image 已固定从 /opt/homebrew/opt 回退定位。
BREW_FORMULAE = ("llvm", "e2fsprogs", "qemu", "openssl@3", "gh", "ninja", "pkgconf")
RUSTUP_INIT_URL = "https://static.rust-lang.org/rustup/dist/aarch64-apple-darwin/rustup-init"
CARGO_BIN = Path.home() / ".cargo" / "bin"


def run(command: Sequence[str | Path], *, environment: dict[str, str] | None = None) -> None:
    """在仓库根目录前台运行一个 host 命令，输出直接流向终端。"""
    subprocess.run([str(argument) for argument in command], cwd=ROOT, env=environment, check=True)


def require_host() -> None:
    """确认 host 是 Apple Silicon macOS 且已安装 Xcode Command Line Tools。

    Raises:
        RuntimeError: host 平台不受支持，或 Command Line Tools 缺失。
    """
    if sys.platform != "darwin" or platform.machine() != "arm64":
        raise RuntimeError("make setup supports Apple Silicon macOS only")
    probe = subprocess.run(["xcode-select", "-p"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if probe.returncode != 0:
        raise RuntimeError("Xcode Command Line Tools are required; run `xcode-select --install` first")


def find_brew() -> Path:
    """返回 Homebrew 可执行文件。

    Raises:
        RuntimeError: Homebrew 未安装；其官方安装器需要交互式 sudo，不由本脚本代为执行。
    """
    brew = shutil.which("brew") or "/opt/homebrew/bin/brew"
    if not Path(brew).is_file():
        raise RuntimeError(
            "Homebrew is required; install it from https://brew.sh and re-run `make setup`"
        )
    return Path(brew)


def install_brew_formulae(brew: Path) -> None:
    """安装缺失的 Homebrew formula；已安装的版本保持不变。"""
    installed = set(
        subprocess.run(
            [str(brew), "list", "--formula", "-1"],
            check=True,
            stdout=subprocess.PIPE,
            text=True,
        ).stdout.split()
    )
    formulae = [name for name in BREW_FORMULAE if name not in installed]
    if formulae:
        run([brew, "install", *formulae])


def install_rust_toolchain() -> None:
    """安装 rustup，并安装 ``rust-toolchain.toml`` 固定的 toolchain、组件与 target。"""
    rustup = shutil.which("rustup") or CARGO_BIN / "rustup"
    if not Path(rustup).is_file():
        DOWNLOADS.mkdir(parents=True, exist_ok=True)
        installer = DOWNLOADS / "rustup-init"
        run(["curl", "--fail", "--location", "--proto", "=https", "--tlsv1.2", "-o", installer, RUSTUP_INIT_URL])
        installer.chmod(0o755)
        run([installer, "-y", "--profile", "minimal", "--default-toolchain", "none"])
        rustup = CARGO_BIN / "rustup"
    environment = dict(os.environ)
    environment["PATH"] = f"{CARGO_BIN}{os.pathsep}{environment.get('PATH', '')}"
    # 无参数 install 读取仓库 rust-toolchain.toml，同时安装其中列出的 components 与 targets。
    run([rustup, "toolchain", "install"], environment=environment)


def install_utm() -> None:
    """构建并缓存固定的 UTM 开发副本；不会覆盖 /Applications 中的用户应用。"""
    ensure_built()


def main() -> int:
    """按顺序执行全部 host setup 步骤，并返回 Make 可直接消费的退出码。"""
    try:
        require_host()
        install_brew_formulae(find_brew())
        install_rust_toolchain()
        install_utm()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"host setup failed: {error}", file=sys.stderr)
        return 1
    if shutil.which("rustup") is None:
        print(f"host setup complete; open a new shell so {CARGO_BIN} is on PATH")
    else:
        print("host setup complete")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
