#!/usr/bin/env python3
"""在 Apple Silicon macOS 上一次性准备 LiteOS host 构建、运行与门禁工具。

本脚本只安装 workflow 已经消费的外部工具，不改变任何构建输入的解析规则：
1. 校验 macOS/arm64 与 Xcode Command Line Tools；
2. 用 Homebrew 安装 Clang/LLVM archive 工具、e2fsprogs、QEMU、RISC-V GCC、OpenSSL、
   git-lfs，以及缺失时的 Node.js；
3. 安装 rustup 并按 ``rust-toolchain.toml`` 安装固定 nightly、组件与 target；
4. 按固定 SHA-256 安装 UTM v4.7.5；
5. 拉取 Git LFS 预置音乐资产。

所有步骤幂等；已满足的步骤直接跳过。
"""

from __future__ import annotations

import hashlib
import os
import platform
import plistlib
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Sequence

from utm_runtime import UTM_APP, UTM_INFO, UTM_VERSION

ROOT = Path(__file__).resolve().parent.parent
DOWNLOADS = ROOT / "target" / "host-setup"
# llvm 与 e2fsprogs 为 keg-only；verify_musl/ext2_image 已固定从 /opt/homebrew/opt 回退定位。
BREW_FORMULAE = ("llvm", "e2fsprogs", "qemu", "riscv64-elf-gcc", "openssl@3", "git-lfs")
UTM_DMG_URL = f"https://github.com/utmapp/UTM/releases/download/v{UTM_VERSION}/UTM.dmg"
UTM_DMG_SHA256 = "a8435c93cfb5f8bbfeea4b134cfad1ac66b67632b75e438c63b1a8ae043bef0e"
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
    # Node.js 只要求 PATH 上存在；保留 fnm/nvm 等用户已有的版本管理方式。
    if shutil.which("node") is None or shutil.which("npm") is None:
        formulae.append("node")
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


def installed_utm_version() -> str | None:
    """返回 /Applications/UTM.app 的版本；未安装时返回 ``None``。"""
    if not UTM_INFO.is_file():
        return None
    with UTM_INFO.open("rb") as stream:
        return plistlib.load(stream).get("CFBundleShortVersionString")


def install_utm() -> None:
    """按固定摘要安装 UTM；已存在其他版本时 fail-stop，不覆盖用户应用。

    Raises:
        RuntimeError: 已安装非固定版本，或下载产物摘要不匹配。
    """
    version = installed_utm_version()
    if version == UTM_VERSION:
        return
    if version is not None or UTM_APP.exists():
        raise RuntimeError(
            f"UTM {UTM_VERSION} is required but {UTM_APP} contains {version or 'an unknown version'}; "
            "remove it and re-run `make setup`"
        )
    DOWNLOADS.mkdir(parents=True, exist_ok=True)
    image = DOWNLOADS / f"UTM-{UTM_VERSION}.dmg"
    if not image.is_file() or hashlib.sha256(image.read_bytes()).hexdigest() != UTM_DMG_SHA256:
        run(["curl", "--fail", "--location", "--proto", "=https", "--tlsv1.2", "-o", image, UTM_DMG_URL])
    digest = hashlib.sha256(image.read_bytes()).hexdigest()
    if digest != UTM_DMG_SHA256:
        image.unlink()
        raise RuntimeError(f"UTM.dmg SHA-256 mismatch: expected {UTM_DMG_SHA256}, got {digest}")
    with tempfile.TemporaryDirectory(prefix="liteos-utm-") as mountpoint:
        run(["hdiutil", "attach", "-nobrowse", "-readonly", "-mountpoint", mountpoint, image])
        try:
            run(["ditto", Path(mountpoint) / "UTM.app", UTM_APP])
        finally:
            run(["hdiutil", "detach", mountpoint])


def pull_lfs_assets() -> None:
    """拉取 rootfs 基线安装的 Git LFS 音乐资产；缺失时 rootfs 只会得到 LFS pointer 文件。"""
    if not (ROOT / ".git").exists():
        return
    run(["git", "lfs", "install", "--local"])
    run(["git", "lfs", "pull"])


def main() -> int:
    """按顺序执行全部 host setup 步骤，并返回 Make 可直接消费的退出码。"""
    try:
        require_host()
        install_brew_formulae(find_brew())
        install_rust_toolchain()
        install_utm()
        pull_lfs_assets()
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
