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
    build_environment, cache_lock, fingerprint, manifest_matches, sha256, publish_directory, publish_runtime_gate, runtime_gate_hit,
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
    # keymap 裁决者 libxkbcommon 及其运行时依赖；-dev 只提供头文件，不进入 guest。
    ("libxkbcommon-1.8.1-r2.apk", "8a37b225bae2144e7caa0e7f01d7d65c6d9a8493ed640ca2a7a3836af70883dc"),
    ("libxkbcommon-dev-1.8.1-r2.apk", "9d31d4c91ed851af2c400d1de6c4e19e5a0064b33d6e6fb80ca674ee561fe056"),
    ("libxml2-2.13.9-r1.apk", "e358e1c32ba6923386263dd641a6e4bcb56a0345fecadef9ef1103892cf3d81d"),
    ("zlib-1.3.2-r1.apk", "2d274cb1b2a36d0e8347d802e6dea8648cab952ea5fae1989ab72e725842a2c3"),
    ("xz-libs-5.8.4-r0.apk", "8a64b55223f29e24f934ccbc8167d535b913bf923dab5e14de7946a7c36091a7"),
)
FIXTURES = ROOT / "scripts/fixtures/wayland"
WORK = ROOT / "target/wayland-runtime"
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
/bin/wayland-probe &
i=0
while [ ! -s /var/tmp/keymap.xkb ] && [ $i -lt 60 ]; do sleep 1; i=$((i+1)); done
/bin/xkb-check /var/tmp/keymap.xkb
wait
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


def build_xkb_check(musl, tool_root: Path) -> Path:
    """用固定 musl sysroot 链接上游 libxkbcommon 构建 keymap 裁决程序；产物只进入 target。"""
    source = FIXTURES / "xkb-check.c"
    payload = {
        "kind": "xkb-check", "source": sha256(source), "sysroot": musl.sysroot_fingerprint,
        "driver": sha256(ROOT / "scripts/musl_clang.py"), "packages": dict(PACKAGES),
    }
    destination = WORK / "xkb-check" / fingerprint(payload)
    binary = destination / "xkb-check"
    if manifest_matches(destination, payload, ("xkb-check",)):
        return binary
    generation = temporary_directory(WORK / "xkb-check", "build")
    env = build_environment()
    env.update({
        "LITEOS_MUSL_CLANG": str(musl.compiler), "LITEOS_MUSL_LLD": str(musl.linker),
        "LITEOS_MUSL_COMPILER_RUNTIME": str(musl.compiler_runtime), "LITEOS_MUSL_SYSROOT": str(musl.install),
    })
    command = [
        sys.executable, ROOT / "scripts/musl_clang.py", source, "-D_GNU_SOURCE", "-std=gnu11", "-Wall", "-Wextra", "-Werror",
        "-fPIE", "-pie", "-I", tool_root / "usr/include", "-L", tool_root / "usr/lib", "-l:libxkbcommon.so.0",
        "-o", generation / "xkb-check",
    ]
    try:
        run([str(argument) for argument in command], ROOT, env)
        write_manifest(generation, payload)
        publish_directory(generation, destination)
    finally:
        shutil.rmtree(generation, ignore_errors=True)
    return binary


def install(image: Path, tool_root: Path, probe: Path, xkb_check: Path, directory: Path) -> None:
    """把上游工具、探针与启动脚本写入私有镜像；保留产品 inittab 的显示服务监督。"""
    recover_ext4_journal(image)
    sources = []
    for path in sorted(tool_root.rglob("*")):
        relative = path.relative_to(tool_root).as_posix()
        if (path.is_file() or path.is_symlink()) and (relative == "usr/bin/wayland-info" or (relative.startswith(("usr/lib/", "lib/")) and ".so" in path.name)):
            sources.append((path, "/" + relative))
    script = directory / "wayland.sh"
    script.write_text(SCRIPT)
    inittab = directory / "inittab"
    inittab.write_text((ROOT / "user/base/inittab").read_text() + "::once:/bin/sh /var/tmp/wayland.sh\n")
    sources += [(probe, "/bin/wayland-probe"), (xkb_check, "/bin/xkb-check"), (script, "/var/tmp/wayland.sh"), (inittab, "/etc/inittab")]
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


def abs_event(axis: str, pixel: int, extent: int) -> dict:
    """virtio 平板的绝对坐标（0..=32767）对应屏幕像素 `pixel`。"""
    return {"type": "abs", "data": {"axis": axis, "value": pixel * 32767 // (extent - 1)}}


def key_events(*steps: tuple[str, bool]) -> list[dict]:
    return [{"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": key}}} for key, down in steps]


def check_input(session) -> None:
    """窗口内的指针移动与点击、Shift+A 都必须以正确的 seat 事件到达客户端。"""
    console = session.console
    # 窗口左上在 (40,40)：指针移到 (100,100) → 窗口局部约 (60,60)；输出 scale 为 1。
    session.qmp.execute("input-send-event", {"events": [abs_event("x", 100, 1280), abs_event("y", 100, 800)]})
    entered = console.wait_for_match(r"wayland-probe: pointer enter x=(-?\d+) y=(-?\d+)\r?\n")
    if not all(59 <= int(value) <= 61 for value in entered.groups()):
        raise RuntimeError(f"pointer entered at the wrong local position: {entered.group(0)!r}")
    session.qmp.execute("input-send-event", {"events": [abs_event("x", 120, 1280), abs_event("y", 110, 800)]})
    console.wait_for(("wayland-probe: pointer motion",))
    session.qmp.execute("input-send-event", {"events": [{"type": "btn", "data": {"down": True, "button": "left"}}]})
    session.qmp.execute("input-send-event", {"events": [{"type": "btn", "data": {"down": False, "button": "left"}}]})
    console.wait_for(("wayland-probe: pointer button 272 pressed", "wayland-probe: pointer button 272 released"))
    # 离开窗口。
    session.qmp.execute("input-send-event", {"events": [abs_event("x", 600, 1280), abs_event("y", 600, 800)]})
    console.wait_for(("wayland-probe: pointer leave",))
    # Shift+A：先是修饰键状态，再是键本身（evdev 30 = A）。
    session.qmp.execute("input-send-event", {"events": key_events(("shift", True), ("a", True), ("a", False), ("shift", False))})
    console.wait_for((
        "wayland-probe: modifiers depressed=1",
        "wayland-probe: key 30 pressed",
        "wayland-probe: key 30 released",
        "wayland-probe: modifiers depressed=0",
    ))


def wait_screen(session, expectations: list[tuple[tuple[int, int], tuple[int, int, int]]], what: str) -> None:
    """限期轮询 scanout，直到所有 `((x, y), 颜色)` 都成立；合成与刷新晚于串口标记，所以不能立即断言。"""
    deadline = time.monotonic() + 10
    while True:
        width, _, pixels = session.screendump()
        wrong = {point: pixels[point[1] * width + point[0]] for point, color in expectations if pixels[point[1] * width + point[0]] != color}
        if not wrong:
            return
        if time.monotonic() > deadline:
            raise RuntimeError(f"{what}: wrong pixels {wrong}")
        time.sleep(0.25)


def pointer_to(session, x: int, y: int) -> None:
    session.qmp.execute("input-send-event", {"events": [abs_event("x", x, 1280), abs_event("y", y, 800)]})


def button(session, down: bool) -> None:
    session.qmp.execute("input-send-event", {"events": [{"type": "btn", "data": {"down": down, "button": "left"}}]})


def tap_steps(*keys: str) -> list[tuple[str, bool]]:
    """组合键的按键序列：先按下修饰键，敲击最后一个键，再按相反顺序释放修饰键。"""
    *modifiers, key = keys
    return [(name, True) for name in modifiers] + [(key, True), (key, False)] + [(name, False) for name in reversed(modifiers)]


def tap(session, *keys: str) -> None:
    session.qmp.execute("input-send-event", {"events": key_events(*tap_steps(*keys))})


def check_window_interaction(session) -> None:
    """标题栏拖动、角缩放、最大化/全屏/最小化与 Alt+Tab 恢复；每一步都由截图裁决窗口的实际位置与大小。"""
    console, window, back = session.console, PROBE_LAST_COLOR, DESKTOP_BACKGROUND
    # 1. 拖动标题栏：窗口 (40,40) 局部 y<30 处按下并拖到 (+60,+40)。
    mark = console.mark()
    pointer_to(session, 100, 50)
    console.wait_for(("wayland-probe: pointer enter",), after=mark)
    mark = console.mark()
    button(session, True)
    console.wait_for(("wayland-probe: requested window drag at",), after=mark)
    pointer_to(session, 160, 90)
    wait_screen(session, [((105, 85), window), ((45, 45), back), ((299, 199), window), ((301, 201), back)], "title-bar drag")
    button(session, False)
    # 2. 拖右下角缩放：窗口现在是 (100,80)-(300,200)；拖角 +100,+100 → 300×220。
    mark = console.mark()
    pointer_to(session, 295, 195)
    console.wait_for(("wayland-probe: pointer enter",), after=mark)
    mark = console.mark()
    button(session, True)
    console.wait_for(("wayland-probe: requested window drag at",), after=mark)
    pointer_to(session, 395, 295)
    console.wait_for(("wayland-probe: resized 300x220",), after=mark)
    console.wait_for_match(r'configure 300x220 states=\["resizing", "activated"\]', after=mark)
    wait_screen(session, [((399, 299), window), ((401, 299), back), ((399, 301), back), ((100, 80), window)], "corner resize")
    mark = console.mark()
    button(session, False)
    console.wait_for_match(r'configure 300x220 states=\["activated"\]', after=mark)
    # 3. 最大化与还原：还原回到最大化之前的位置与尺寸。
    mark = console.mark()
    tap(session, "m")
    console.wait_for(("wayland-probe: resized 1280x800",), after=mark)
    console.wait_for_match(r'configure 1280x800 states=\["maximized", "activated"\]', after=mark)
    wait_screen(session, [((0, 0), window), ((1279, 799), window)], "maximize")
    mark = console.mark()
    tap(session, "m")
    console.wait_for(("wayland-probe: resized 300x220",), after=mark)
    wait_screen(session, [((0, 0), back), ((100, 80), window), ((399, 299), window), ((401, 299), back)], "restore from maximize")
    # 4. 全屏与还原。
    mark = console.mark()
    tap(session, "f")
    console.wait_for_match(r'configure 1280x800 states=\["fullscreen", "activated"\]', after=mark)
    wait_screen(session, [((0, 0), window), ((1279, 799), window)], "fullscreen")
    mark = console.mark()
    tap(session, "f")
    console.wait_for(("wayland-probe: resized 300x220",), after=mark)
    wait_screen(session, [((0, 0), back), ((100, 80), window)], "restore from fullscreen")
    # 5. 最小化：窗口消失；Alt+Tab 恢复（只有一个窗口时也必须能恢复）。
    tap(session, "n")
    wait_screen(session, [((150, 150), back), ((100, 80), back)], "minimize")
    tap(session, "alt", "tab")
    wait_screen(session, [((150, 150), window), ((100, 80), window)], "alt+tab restore")


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
        xkb_check = build_xkb_check(musl, tool_root)
        stamp = ROOT / "target" / "verify-gates" / "wayland-aarch64.json"
        payload = runtime_gate_payload(
            "wayland", RECIPE_VERSION,
            (image, probe, xkb_check, *archives, ROOT / "scripts/fixtures/wayland/xkb-check.c", ROOT / "scripts/display_gate.py", ROOT / "scripts/qemu_gate.py", Path(__file__).resolve()),
        )
        if runtime_gate_hit(stamp, payload, (image,)):
            print("wayland verification cache hit")
            return 0
        with tempfile.TemporaryDirectory(prefix="liteos-wayland-install-") as scratch:
            private = Path(scratch) / image.name
            shutil.copyfile(image, private)
            install(private, tool_root, probe, xkb_check, Path(scratch))
            with display_session(private) as session:
                session.console.wait_for(("WAYLAND_INFO_DONE", "wayland-probe: done", "XKB_OK"))
                text = session.console.text
                check_info(text[text.index("interface: "):text.index("WAYLAND_INFO_DONE")])
                for needed in ("wayland-probe: bound", "wayland-probe: configured", "wayland-probe: frame 4 presented", "compositor: windows=1", "wayland-probe: keymap ", "wayland-probe: keyboard enter", "XKB_OK"):
                    if needed not in text:
                        raise RuntimeError(f"missing `{needed}`\n{text[-2000:]}")
                width, height, pixels = session.screendump()
                check_screen(width, height, pixels)
                check_input(session)
                check_window_interaction(session)
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
        print("wayland gate: globals, keymap, pointer/keyboard order, window drag/resize/maximize/fullscreen/minimize/Alt+Tab, composited and closed by Alt+F4")
        publish_runtime_gate(stamp, payload)
    except RuntimeError as error:
        print(f"wayland verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
