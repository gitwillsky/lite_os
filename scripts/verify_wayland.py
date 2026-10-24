#!/usr/bin/env python3
"""Wayland 服务端运行时门禁：上游 `wayland-info` 枚举全局对象，客户端探针走完整窗口生命周期并被合成上屏。

1. 在私有镜像里放入固定的 Alpine `wayland-utils`（及 libwayland-client、libffi、libdrm、libpciaccess）与
   `wayland-probe`（本仓库的 shm/xdg 客户端），产品 rootfs 不继承它们。
2. guest 里 `wayland-info` 的输出必须列出本服务端实现的全局对象、`wl_output` 的模式与 scale、`wl_shm` 的格式。
3. `wayland-probe` 报告 5 帧都收到了 frame callback；QMP `screendump` 里窗口矩形是最后一帧的颜色，
   其外是桌面背景色，位置是层叠放置的第一个窗口。
4. 注入 Alt+F4：compositor 向活动窗口发 `xdg_toplevel.close`，探针退出，窗口消失、桌面恢复。
"""

from __future__ import annotations

import argparse
import re
import shutil
import sys
import tarfile
import tempfile
import time
from pathlib import Path

from apk_cache import SOURCE, download
from build_cache import (
    cache_lock, fingerprint, manifest_matches, publish_directory, publish_runtime_gate, runtime_gate_hit,
    runtime_gate_payload, temporary_directory, write_manifest,
)
from build_target import BuildTarget, acceleration_from_environment, target_from_environment
from display_gate import display_session
from ext4_image import find_debugfs, recover_ext4_journal
from verify_busybox import build_rust_user_program
from verify_musl import cached_musl_paths, find_compiler, run

ROOT = Path(__file__).resolve().parent.parent
RECIPE_VERSION = 1
# 只注入私有 gate 镜像：上游 wayland-info 及其运行时依赖（musl 由产品提供）。
PACKAGES = (
    ("wayland-utils-1.2.0-r0.apk", "f228b6562d731433850d7ecafd3542812d4b8e3abf62322868fc2c8adfcba501"),
    ("wayland-libs-client-1.23.1-r3.apk", "9481b11de37951462cf26a8309e410346455f67b17f7f2d48f0d2326c522ebee"),
    ("libffi-3.4.8-r0.apk", "9391f60a14c146655deaf65115563bc8dcd749cf0f93ec567e6443f2ed7d3bfc"),
    ("libdrm-2.4.124-r0.apk", "e966fb3fc167255ed3de436134425e6207a38438e4df2e54ee807431ab4230f3"),
    ("libpciaccess-0.18.1-r0.apk", "22d0bb3ebc355338468b587325b5b624b9cf9ba10151918b48c6f7a575bb248a"),
)
# 与 user/compositor/src/bin/wayland-probe.rs 的常量一致。
PROBE_SIZE = (200, 120)
PROBE_LAST_COLOR = (0x20, 0xC0, 0xC0)
DESKTOP_BACKGROUND = (0x3A, 0x6E, 0xA5)
FIRST_WINDOW_ORIGIN = 40
SCRIPT = """#!/bin/sh
export XDG_RUNTIME_DIR=/run/liteos
i=0
while [ ! -S $XDG_RUNTIME_DIR/wayland-0 ] && [ $i -lt 60 ]; do sleep 1; i=$((i+1)); done
/usr/bin/wayland-info
echo WAYLAND_INFO_DONE
exec /bin/wayland-probe
"""
# 上游 wayland-info 对齐输出：全局对象行、shm 格式（fourcc）与 wl_output 的几何、名字和模式。
EXPECTED_INFO = (
    r"interface: 'wl_compositor',\s+version:\s+6,\s+name:\s+1",
    r"interface: 'wl_shm',\s+version:\s+1,\s+name:\s+2",
    r"interface: 'xdg_wm_base',\s+version:\s+3,\s+name:\s+3",
    r"interface: 'wl_output',\s+version:\s+4,\s+name:\s+4",
    r"'AR24'",
    r"'XR24'",
    r"make: 'LiteOS', model: 'Virtual Display'",
    r"name: LITEOS-1",
    r"width: 1280 px, height: 800 px, refresh: 60\.000 Hz",
    r"flags: current preferred",
)


def tools() -> tuple[Path, tuple[Path, ...]]:
    """返回已校验并解包的固定 Wayland 客户端工具；错误时不发布源树。"""
    archives = tuple(download(name, digest) for name, digest in PACKAGES)
    payload = {"kind": "wayland-consumer-tools", "packages": dict(PACKAGES)}
    destination = SOURCE / "wayland-tools" / fingerprint(payload)
    with cache_lock(SOURCE / ".wayland-tools.lock"):
        if manifest_matches(destination, payload, ("usr/bin/wayland-info",)):
            return destination, archives
        temporary = temporary_directory(SOURCE / "wayland-tools", "extract")
        try:
            for archive in archives:
                with tarfile.open(archive, "r:gz") as package:
                    package.extractall(temporary, filter="data")
            write_manifest(temporary, payload)
            publish_directory(temporary, destination)
        finally:
            shutil.rmtree(temporary, ignore_errors=True)
    return destination, archives


def install(image: Path, tool_root: Path, probe: Path, directory: Path) -> None:
    """把上游工具、探针与启动脚本写入私有镜像；保留产品 inittab 的显示服务监督。"""
    recover_ext4_journal(image)
    sources = []
    for path in sorted(tool_root.rglob("*")):
        relative = path.relative_to(tool_root).as_posix()
        if (path.is_file() or path.is_symlink()) and (relative == "usr/bin/wayland-info" or (relative.startswith("usr/lib/") and ".so" in path.name)):
            sources.append((path, "/" + relative))
    script = directory / "wayland.sh"
    script.write_text(SCRIPT)
    inittab = directory / "inittab"
    inittab.write_text((ROOT / "user/base/inittab").read_text() + "::once:/bin/sh /var/tmp/wayland.sh\n")
    sources += [(probe, "/bin/wayland-probe"), (script, "/var/tmp/wayland.sh"), (inittab, "/etc/inittab")]
    directories = set()
    for _, destination in sources:
        parent = Path(destination).parent
        while str(parent) != "/":
            directories.add(str(parent))
            parent = parent.parent
    commands = [f"mkdir {path}" for path in sorted(directories, key=lambda path: (path.count("/"), path))]
    for source, destination in sources:
        commands.append(f"rm {destination}")
        if source.is_symlink():
            commands.append(f"symlink {destination} {source.readlink()}")
        else:
            commands.extend((f"write {source} {destination}", f"set_inode_field {destination} mode 0100{source.stat().st_mode & 0o777:o}"))
    batch = directory / "install.debugfs"
    batch.write_text("\n".join(commands) + "\n")
    run([str(find_debugfs()), "-w", "-f", str(batch), str(image)], ROOT)


def check_info(text: str) -> None:
    """`wayland-info` 的输出必须包含全部期望片段，缺失时指出是哪一条。"""
    missing = [pattern for pattern in EXPECTED_INFO if re.search(pattern, text) is None]
    if missing:
        raise RuntimeError(f"wayland-info output is missing {missing}\n{text[-2500:]}")


def check_screen(width: int, height: int, pixels: list[tuple[int, int, int]]) -> None:
    """窗口矩形必须是最后一帧的颜色，矩形外一圈是桌面背景。"""
    at = lambda x, y: pixels[y * width + x]
    left, top = FIRST_WINDOW_ORIGIN, FIRST_WINDOW_ORIGIN
    right, bottom = left + PROBE_SIZE[0], top + PROBE_SIZE[1]
    expectations = {
        "window top-left": (at(left, top), PROBE_LAST_COLOR),
        "window bottom-right": (at(right - 1, bottom - 1), PROBE_LAST_COLOR),
        "window center": (at((left + right) // 2, (top + bottom) // 2), PROBE_LAST_COLOR),
        "left of window": (at(left - 1, top), DESKTOP_BACKGROUND),
        "above window": (at(left, top - 1), DESKTOP_BACKGROUND),
        "right of window": (at(right, top), DESKTOP_BACKGROUND),
        "below window": (at(left, bottom), DESKTOP_BACKGROUND),
        "screen corner": (at(0, 0), DESKTOP_BACKGROUND),
    }
    wrong = {name: f"got {got}, expected {want}" for name, (got, want) in expectations.items() if got != want}
    if wrong:
        raise RuntimeError(f"composited screen is wrong: {wrong}")


def default_image(target: BuildTarget) -> Path:
    return ROOT / "target" / "rootfs" / f"{target.arch}.img"


def main() -> int:
    target = target_from_environment()
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--image", type=Path, default=default_image(target))
    args = parser.parse_args()
    if target.arch != "aarch64" or acceleration_from_environment() != "hvf":
        print(f"wayland gate skipped: {target.arch} has no virtio-gpu display path in the gate")
        return 0
    image = args.image.resolve()
    try:
        if not image.is_file():
            raise RuntimeError(f"rootfs image is missing: {image}")
        musl = cached_musl_paths(find_compiler())
        probe = build_rust_user_program(musl, "compositor", "wayland-probe", "wayland-probe", 1)
        tool_root, archives = tools()
        stamp = ROOT / "target" / "verify-gates" / "wayland-aarch64.json"
        payload = runtime_gate_payload(
            "wayland", RECIPE_VERSION,
            (image, probe, *archives, ROOT / "scripts/display_gate.py", ROOT / "scripts/qemu_gate.py", Path(__file__).resolve()),
        )
        if runtime_gate_hit(stamp, payload, (image,)):
            print("wayland verification cache hit")
            return 0
        with tempfile.TemporaryDirectory(prefix="liteos-wayland-install-") as scratch:
            private = Path(scratch) / image.name
            shutil.copyfile(image, private)
            install(private, tool_root, probe, Path(scratch))
            with display_session(private) as session:
                session.console.wait_for(("WAYLAND_INFO_DONE", "wayland-probe: done"))
                text = session.console.text
                check_info(text[text.index("interface: "):text.index("WAYLAND_INFO_DONE")])
                for needed in ("wayland-probe: bound", "wayland-probe: configured", "wayland-probe: frame 4 presented", "compositor: windows=1"):
                    if needed not in text:
                        raise RuntimeError(f"missing `{needed}`\n{text[-2000:]}")
                width, height, pixels = session.screendump()
                check_screen(width, height, pixels)
                # Alt+F4 → compositor 对活动窗口发 xdg_toplevel.close → 探针退出 → 窗口消失、桌面恢复。
                keys = [("alt", True), ("f4", True), ("f4", False), ("alt", False)]
                session.qmp.execute(
                    "input-send-event",
                    {"events": [{"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": key}}} for key, down in keys]},
                )
                session.console.wait_for(("compositor: close requested", "wayland-probe: close requested", "compositor: windows=0"))
                # 标记先于下一次合成与 scanout 刷新：限期轮询直到桌面恢复，而不是立即断言。
                deadline = time.monotonic() + 10
                while True:
                    width, height, pixels = session.screendump()
                    gone = pixels[FIRST_WINDOW_ORIGIN * width + FIRST_WINDOW_ORIGIN + 10]
                    if gone == DESKTOP_BACKGROUND:
                        break
                    if time.monotonic() > deadline:
                        raise RuntimeError(f"closed window still on screen: {gone}")
                    time.sleep(0.25)
        print("wayland gate: upstream wayland-info lists the globals; probe window composited at the expected pixels and closed by Alt+F4")
        publish_runtime_gate(stamp, payload)
    except RuntimeError as error:
        print(f"wayland verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
