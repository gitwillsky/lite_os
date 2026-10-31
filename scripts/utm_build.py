#!/usr/bin/env python3
"""构建唯一的 UTM 开发运行时；输入可离线复用，临时源码与编译目录随构建清理。"""
from __future__ import annotations

import hashlib
import fcntl
import json
import os
import plistlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path

from local_inputs import INPUTS

ROOT = Path(__file__).resolve().parent.parent
HOST = ROOT / "target/host"
UTM_APP = HOST / "UTM.app"
UTM_VERSION = "5.0.8"
USB_BRIDGE = HOST / "bin/usb-dac-bridge"
USB_REDIRECT = HOST / "bin/usbredirect"
PATCH = ROOT / "scripts/patches/utm-xhci-iso.patch"
BRIDGE_SOURCE = ROOT / "scripts/host/usb_dac_bridge.c"
UTM_REVISION = "ebce1808d7507ac03bd43af9d48f2c2e64071e91"
ARTIFACT_RUN = 40995597571
DOWNLOADS = INPUTS / "utm"
MODULE = Path("Contents/Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu")
# 固定 CI 依赖避免混入 Homebrew 的不同 ABI；归档保留在 .local/inputs，target 清理后仍可重建。
SOURCES = (
    (f"UTM-{UTM_REVISION}.tar.gz", f"https://github.com/utmapp/UTM/archive/{UTM_REVISION}.tar.gz", "81c2bb6026caa974118a06310f22c48b4181ca84775e2424451fbdc88c73dbe3"),
    ("qemu-10.0.12-utm.tar.xz", "https://github.com/utmapp/qemu/releases/download/v10.0.12-utm/qemu-10.0.12-utm.tar.xz", "7c9605290b34152debb842e965a55d2e4fbba4793e536ab677b4027e5dc0ba1a"),
    (f"sysroot-macos-arm64-{ARTIFACT_RUN}.zip", "12966619769", "4acb6b0495c3f5192c23ded98e7cc97858164b6093d8d0125a2f8366cb742c2a"),
    (f"utm-macos-arm64-{ARTIFACT_RUN}.zip", "12969803177", "1908490406f68c2dfb4f85c1b3e3996bd6fb162ef1b7c88404c1e4e554dd01ca"),
)


def _digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def _app_key() -> str:
    value = hashlib.sha256(json.dumps(SOURCES).encode())
    for path in (Path(__file__), PATCH):
        value.update(path.read_bytes())
    return value.hexdigest()


def _key() -> str:
    return hashlib.sha256(_app_key().encode() + BRIDGE_SOURCE.read_bytes()).hexdigest()


def _run(command: list[str | Path], log, *, cwd: Path | None = None, env=None) -> None:
    result = subprocess.run([str(x) for x in command], cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT)
    if result.returncode:
        raise RuntimeError(f"UTM build command failed ({result.returncode}): {command[0]}; see target/logs/utm-build.log")


def _download(name: str, url: str, digest: str) -> Path:
    path = DOWNLOADS / name
    if path.is_file() and _digest(path) == digest:
        return path
    DOWNLOADS.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="liteos-utm-download-") as temporary:
        fresh = Path(temporary) / name
        print(f"UTM input: {name}", flush=True)
        if url.isdecimal():
            with fresh.open("wb") as output:
                result = subprocess.run(["gh", "api", f"repos/utmapp/UTM/actions/artifacts/{url}/zip"], stdout=output)
            if result.returncode:
                raise RuntimeError("UTM CI input download failed; run `gh auth login`. Expired pinned artifacts require an explicit baseline update or the retained offline inputs.")
        else:
            with urllib.request.urlopen(url, timeout=60) as response, fresh.open("wb") as output:
                shutil.copyfileobj(response, output)
        if _digest(fresh) != digest:
            raise RuntimeError(f"UTM input SHA-256 mismatch: {name}")
        shutil.copyfile(fresh, path)
    return path


def _unpack(path: Path, destination: Path) -> None:
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            archive.extractall(destination)
        return
    with tarfile.open(path) as archive:
        for member in archive:
            # 只构建 QEMU host framework，固件使用 UTM 随包产物；不提取 EDK2 的外部 X11 symlink。
            if path.name.startswith("qemu-") and "/roms/" in member.name:
                continue
            archive.extract(member, destination, filter="data")


def _sign_development(app: Path, source: Path, work: Path, log) -> None:
    nested = app / "Contents/XPCServices/QEMUHelper.xpc"
    for template, target in (
        ("QEMULauncher/QEMULauncher-unsigned.entitlements", nested / "Contents/MacOS/QEMULauncher.app"),
        ("QEMURenderServer/QEMURenderServer-unsigned.entitlements", nested / "Contents/MacOS/QEMURenderServer.app"),
        ("QEMUHelper/QEMUHelper-unsigned.entitlements", nested),
        ("utmctl/utmctl-unsigned.entitlements", app / "Contents/MacOS/utmctl"),
        ("Platform/macOS/macOS-unsigned.entitlements", app),
    ):
        entitlements = plistlib.loads((source / template).read_bytes())
        # unsigned archive 的 AppGroupIdentifier 为 invalid.*，UTM 已按该值使用自身容器。
        # 临时签名没有 Team ID；保留未展开的 group 会被 macOS 容器初始化拒绝。
        entitlements.pop("com.apple.security.application-groups", None)
        data = plistlib.dumps(entitlements).decode().replace("$(PRODUCT_BUNDLE_PREFIX:default=com.utmapp)", "com.utmapp")
        path = work / (target.name + ".entitlements")
        path.write_text(data)
        _run(["codesign", "--force", "--sign", "-", "--timestamp=none", "--options", "runtime", "--entitlements", path, target], log)
    _run(["codesign", "--verify", "--deep", "--strict", app], log)


def _build() -> Path:
    key = _key()
    manifest = HOST / "utm-build.json"
    products = (UTM_APP / MODULE, USB_BRIDGE, USB_REDIRECT)
    state = {}
    try:
        state = json.loads(manifest.read_text())
        if state["key"] == key and all(_digest(p) == state["products"][str(p.relative_to(HOST))] for p in products):
            return UTM_APP
    except (OSError, ValueError, KeyError):
        pass
    try:
        app_valid = state.get("app_key") == _app_key() and _digest(UTM_APP / MODULE) == state["products"][str((UTM_APP / MODULE).relative_to(HOST))]
    except (OSError, KeyError):
        app_valid = False
    if sys.platform != "darwin" or os.uname().machine != "arm64":
        raise RuntimeError("UTM development runtime requires Apple Silicon macOS")
    for tool in ("clang", "ninja", "pkg-config", "gh", "codesign"):
        if shutil.which(tool) is None:
            raise RuntimeError(f"{tool} is required; run `make setup`")
    # 映射中的 app 不能被另一构建替换；唯一发布者由 workflow 的前台进程持有。
    executable = str(UTM_APP / "Contents/MacOS/UTM")
    processes = subprocess.check_output(["ps", "-axo", "comm="], text=True).splitlines()
    if executable in (line.strip() for line in processes):
        raise RuntimeError("Quit the LiteOS development UTM before rebuilding its host runtime")
    inputs = [_download(*entry) for entry in SOURCES]
    HOST.mkdir(parents=True, exist_ok=True)
    log_path = ROOT / "target/logs/utm-build.log"
    log_path.parent.mkdir(parents=True, exist_ok=True)
    print("Building patched UTM host runtime (cached after this build)", flush=True)
    with tempfile.TemporaryDirectory(prefix="liteos-utm-build-") as temporary, log_path.open("w") as log:
        work = Path(temporary)
        for path in inputs:
            if app_valid and (path.name.startswith("qemu-") or path.name.startswith("utm-macos-")):
                continue
            _unpack(path, work)
        _unpack(work / "sysroot.tgz", work)
        if not app_valid:
            _unpack(work / "UTM.xcarchive.tgz", work)
        source = work / f"UTM-{UTM_REVISION}"
        qemu = work / "qemu-10.0.12-utm"
        sdk = work / "sysroot-macOS-arm64"
        if not app_valid:
            _run(["patch", "-p1", "-i", source / "patches/qemu-10.0.12-utm.patch"], log, cwd=qemu)
            _run(["patch", "-p1", "-i", PATCH], log, cwd=qemu)
            venv = work / "venv"
            _run([sys.executable, "-m", "venv", venv], log)
            python = venv / "bin/python3"
            _run([python, "-m", "pip", "install", "meson==1.5.0", "pycotap==1.3.1"], log)
            pc = work / "pkgconfig"
            pc.mkdir()
            # libgcrypt 的 CI config-tool 记录旧安装路径；用同一 SDK 的元数据防止探测宿主 Homebrew。
            (pc / "libgcrypt.pc").write_text(f"prefix={sdk}\nName: libgcrypt\nDescription: UTM CI libgcrypt\nVersion: 1.8.4\nLibs: -L${{prefix}}/lib -lgcrypt -lgpg-error\nCflags: -I${{prefix}}/include\n")
            pkg = work / "pkg-config"
            # shell 仅承载构建期工具参数；路径逐个引用，不能把 checkout 路径当 shell 代码。
            import shlex
            pkg.write_text("#!/bin/sh\nexport PKG_CONFIG_PATH=" + shlex.quote(str(pc)) + "\nexport PKG_CONFIG_LIBDIR=" + shlex.quote(f"{sdk}/lib/pkgconfig:{sdk}/share/pkgconfig") + "\nexec " + shlex.quote(shutil.which("pkg-config")) + " --define-variable=prefix=" + shlex.quote(str(sdk)) + ' "$@"\n')
            pkg.chmod(0o700)
            build = work / "qemu-build"
            build.mkdir()
            environment = dict(os.environ, PKG_CONFIG=str(pkg), PKG_CONFIG_PATH=str(pc), PKG_CONFIG_LIBDIR=f"{sdk}/lib/pkgconfig:{sdk}/share/pkgconfig", MACOSX_DEPLOYMENT_TARGET="12.0")
            _run([qemu / "configure", f"--python={python}", "--target-list=aarch64-softmmu", "--enable-shared-lib", "--enable-hvf", "--enable-tcg", "--enable-spice", "--enable-opengl", "--enable-virglrenderer", "--enable-usb-redir", "--enable-libusb", "--enable-slirp", "--disable-cocoa", "--disable-sdl", "--disable-debug-info", "--disable-docs", "--disable-tools", "--disable-guest-agent", "--disable-capstone", f"--extra-cflags=-I{sdk}/include", f"--extra-ldflags=-L{sdk}/lib"], log, cwd=build, env=environment)
            # 该 shared target 由 Objective-C 链接，QEMU configure 的 c_link_args 不传递给它。
            _run([build / "pyvenv/bin/meson", "configure", f"-Dobjc_link_args=-L{sdk}/lib", "."], log, cwd=build, env=environment)
            _run(["ninja", "-j", str(max(1, (os.cpu_count() or 2) - 2)), "libqemu-aarch64-softmmu.dylib"], log, cwd=build, env=environment)
            library = sdk / "lib/libqemu-aarch64-softmmu.dylib"
            shutil.copy2(build / library.name, library)
            _run([source / "scripts/fixup.sh", "-p", "macos", "-s", sdk, "-m", "12.0", library], log)
            archive = work / "UTM.xcarchive"
            app = archive / "Products/Applications/UTM.app"
            _run(["ditto", sdk / "Frameworks/qemu-aarch64-softmmu.framework", app / "Contents/Frameworks/qemu-aarch64-softmmu.framework"], log)
            package = work / "package"
            package.mkdir()
            _run([source / "scripts/package_mac.sh", "unsigned", archive, package], log)
            mount = work / "mount"
            mount.mkdir()
            _run(["hdiutil", "attach", "-readonly", "-nobrowse", "-mountpoint", mount, package / "UTM.dmg"], log)
            staged = work / "staged/UTM.app"
            try:
                _run(["ditto", mount / "UTM.app", staged], log)
            finally:
                _run(["hdiutil", "detach", mount], log)
            _sign_development(staged, source, work, log)
        binaries = work / "bin"
        binaries.mkdir()
        redirect = binaries / "usbredirect"
        shutil.copy2(sdk / "bin/usbredirect", redirect)
        bridge = binaries / USB_BRIDGE.name
        _run(["clang", "-Wall", "-Wextra", "-Werror", f"-I{sdk}/include/libusb-1.0", BRIDGE_SOURCE, f"-L{sdk}/lib", "-lusb-1.0", "-o", bridge], log)
        for binary in (bridge, redirect):
            _run([source / "scripts/fixup.sh", "-p", "macos", "-s", sdk, "-i", binary], log)
            _run(["install_name_tool", "-add_rpath", "@executable_path/../UTM.app/Contents/Frameworks", binary], log)
            _run(["codesign", "--force", "--sign", "-", "--timestamp=none", binary], log)
        # 只有成功的整组产物才发布 manifest；失败的构建不会被下一次启动当作 cache hit。
        if not app_valid:
            if UTM_APP.exists():
                shutil.rmtree(UTM_APP)
            _run(["ditto", staged, UTM_APP], log)
        (HOST / "bin").mkdir(exist_ok=True)
        for binary in (bridge, redirect):
            shutil.copy2(binary, HOST / "bin" / binary.name)
        state = {"key": key, "app_key": _app_key(), "utm_revision": UTM_REVISION, "products": {str(p.relative_to(HOST)): _digest(p) for p in products}}
        manifest.write_text(json.dumps(state, indent=2) + "\n")
    return UTM_APP


def ensure_built() -> Path:
    """返回固定输入对应的 UTM；失败不会回退到未修复版本。

    同一 checkout 的发布锁覆盖校验和构建，避免两个 make 同时替换 app 或 manifest。

    Raises:
        RuntimeError: 输入、依赖或构建失败，或另一进程正在构建。
    """
    HOST.mkdir(parents=True, exist_ok=True)
    with (HOST / "utm-build.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise RuntimeError("Another process is building the UTM host runtime") from error
        return _build()


if __name__ == "__main__":
    print(ensure_built())
