#!/usr/bin/env python3
"""显示服务运行时门禁：compositor 接管内核启动画面，并从 evdev 读到指针与按键。

在 QEMU（virtio-gpu、键盘、平板，无窗口）里冷启动产品 rootfs：

1. 串口出现 compositor 的首帧与输入就绪标记。
2. 经 QMP `screendump` 取回 scanout，与 `LBS1` 参考解码器按同一整数缩放居中规则算出的帧逐像素比对；
   两端（内核 `boot_screen` 与 compositor）任何一方的缩放、居中或填充规则漂移，这里都会失败。
3. 经 QMP 注入一次平板移动与一次按键，要求 compositor 报告各自的首个事件。
"""

from __future__ import annotations

import argparse
import struct
import sys
from pathlib import Path

from build_cache import publish_runtime_gate, runtime_gate_hit, runtime_gate_payload
from build_target import BuildTarget, acceleration_from_environment, target_from_environment
from classic_assets import boot_scene
from display_gate import display_session
from pack_boot_scene import HEIGHT, WIDTH, unpack

ROOT = Path(__file__).resolve().parent.parent
RECIPE_VERSION = 1
# 与 boot-scene crate 的 KERNEL_PROGRESS_PERMILLE 一致：内核交接给 compositor 时的进度。
HANDOFF_PERMILLE = 400
STARTUP_MARKERS = ("compositor: first frame presented", "compositor: input ready")


def default_image(target: BuildTarget) -> Path:
    """返回目标隔离的只读 rootfs baseline。"""
    return ROOT / "target" / "rootfs" / f"{target.arch}.img"


def gate_inputs(target: BuildTarget, image: Path) -> tuple[Path, ...]:
    """缓存输入：任何可能改变画面或输入路径的比特。"""
    return (
        image,
        ROOT / target.kernel_boot_artifact(),
        boot_scene(),
        ROOT / "scripts/qemu_gate.py",
        ROOT / "scripts/display_gate.py",
        ROOT / "scripts/pack_boot_scene.py",
        Path(__file__).resolve(),
    )


def expected_frame(scene: bytes, width: int, height: int) -> list[tuple[int, int, int]]:
    """按内核与 compositor 共同的规则算出接管后的整屏 RGB。

    整数倍 `min(W/640, H/480)` 放大并居中，位图外为黑；进度条从槽内部左端涂到 `HANDOFF_PERMILLE`。
    """
    _, _, pixels = unpack(scene)
    scale = min(width // WIDTH, height // HEIGHT)
    if scale == 0:
        raise RuntimeError("screen is smaller than the boot scene")
    left, top = (width - WIDTH * scale) // 2, (height - HEIGHT * scale) // 2
    frame = [(0, 0, 0)] * (width * height)
    for y in range(HEIGHT * scale):
        row = (top + y) * width + left
        source = (y // scale) * WIDTH
        for x in range(WIDTH * scale):
            frame[row + x] = pixels[source + x // scale]
    slot_x, slot_y, slot_width, slot_height = struct.unpack_from("<HHHH", scene, 12)
    color = struct.unpack_from("<I", scene, 20)[0]
    fill = (color >> 16 & 255, color >> 8 & 255, color & 255)
    filled = slot_width * scale * HANDOFF_PERMILLE // 1000
    for y in range(slot_height * scale):
        row = (top + slot_y * scale + y) * width + left + slot_x * scale
        frame[row:row + filled] = [fill] * filled
    return frame


def first_difference(actual: list, expected: list, width: int) -> str:
    """返回第一个不同像素的坐标与两侧颜色，便于定位缩放/居中/填充漂移。"""
    for index, (a, b) in enumerate(zip(actual, expected)):
        if a != b:
            return f"first difference at ({index % width},{index // width}): got {a}, expected {b}"
    return "sizes differ"


def run_gate(image: Path) -> None:
    scene = boot_scene().read_bytes()
    with display_session(image) as session:
        session.console.wait_for(STARTUP_MARKERS)
        width, height, actual = session.screendump()
        expected = expected_frame(scene, width, height)
        if actual != expected:
            raise RuntimeError(f"handoff frame differs from the reference: {first_difference(actual, expected, width)}")
        session.qmp.execute(
            "input-send-event",
            {"events": [
                {"type": "abs", "data": {"axis": "x", "value": 20000}},
                {"type": "abs", "data": {"axis": "y", "value": 12000}},
            ]},
        )
        session.console.wait_for(("compositor: first pointer event",))
        session.qmp.execute(
            "input-send-event",
            {"events": [{"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "a"}}}]},
        )
        session.console.wait_for(("compositor: first key event",))
        print(f"display gate: {width}x{height} handoff frame is pixel-exact; pointer and key input observed")


def main() -> int:
    target = target_from_environment()
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--image", type=Path, default=default_image(target))
    args = parser.parse_args()
    if target.arch != "aarch64" or acceleration_from_environment() != "hvf":
        print(f"display gate skipped: {target.arch} has no virtio-gpu display path in the gate")
        return 0
    image = args.image.resolve()
    try:
        if not image.is_file():
            raise RuntimeError(f"rootfs image is missing: {image}")
        stamp = ROOT / "target" / "verify-gates" / "display-aarch64.json"
        payload = runtime_gate_payload("display", RECIPE_VERSION, gate_inputs(target, image))
        if runtime_gate_hit(stamp, payload, (image,)):
            print("display verification cache hit")
            return 0
        run_gate(image)
        publish_runtime_gate(stamp, payload)
    except RuntimeError as error:
        print(f"display verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
