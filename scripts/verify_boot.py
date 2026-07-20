#!/usr/bin/env python3
"""Run deterministic non-test QEMU cold boots against several DTB hart sets."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from build_cache import publish_runtime_gate, runtime_gate_hit, runtime_gate_payload
from build_target import BuildTarget, target_from_environment
from qemu_gate import boot as boot_image, cpu_topology_markers
from verify_busybox import cached_busybox_binary
from verify_musl import cached_musl_paths, find_compiler

ROOT = Path(__file__).resolve().parent.parent
# musl 和 BusyBox runtime gates 已分别覆盖 1/8 hart；这里只保留独有的非幂次 3-hart DTB。
SMP_CONFIGURATIONS = (3,)


def default_image(target: BuildTarget) -> Path:
    """返回目标隔离的只读 rootfs baseline。"""
    return ROOT / "target" / "rootfs" / f"{target.arch}.img"


def gate_inputs(
    target: BuildTarget,
    image: Path,
    busybox: Path,
    musl_install: Path,
) -> tuple[Path, ...]:
    """返回 boot stamp 的 ELF、启动产物与完整目标输入。"""
    kernel_elf = ROOT / target.kernel_elf()
    kernel_boot_artifact = ROOT / target.kernel_boot_artifact()
    artifacts = [
        image,
        kernel_elf,
        busybox,
        musl_install / "usr/lib/libc.so",
        ROOT / "user/base/inittab",
        ROOT / "create_fs.py",
        ROOT / "scripts/verify_busybox.py",
        Path(__file__).resolve(),
        ROOT / "scripts/qemu_gate.py",
    ]
    if kernel_boot_artifact != kernel_elf:
        artifacts.append(kernel_boot_artifact)
    if target.requires_bootloader:
        artifacts.append(
            ROOT
            / "bootloader"
            / "target"
            / target.kernel_triple
            / "release"
            / "bootloader"
        )
    return tuple(artifacts)


def boot(image: Path, smp: int) -> None:
    """冷启动指定 rootfs，并核对 logical CPU topology 与 BusyBox init。

    Args:
        image: 只读基准 rootfs；QEMU helper 会创建私有可写副本。
        smp: 本次 QEMU 向 guest 暴露的 hart 数。

    Returns:
        None；全部启动标记出现后返回。

    Raises:
        RuntimeError: QEMU 启动失败、超时或缺少预期标记。
    """
    markers = (*cpu_topology_markers(smp), "init started: BusyBox v1.37.0")
    boot_image(image, smp, markers)


def boot_interactive_devices(image: Path) -> None:
    """在无 host 窗口下验证 run-gui 的 GPU、输入设备拓扑与桌面全链路。

    `desktop: mode` 证明 modeset 完成；`desktop: client connected` 与
    `terminal: connected` 证明 AF_UNIX + SCM_RIGHTS 握手成立；`desktop: surface`
    证明客户端 dumb buffer 已映射进合成器；`terminal: shell spawned` 证明
    终端 PTY 监督就绪。缺失任一 marker 表示桌面栈对应环节断裂。
    """
    boot_image(
        image,
        1,
        (
            "VirtIO input event0",
            "VirtIO input event1",
            "VirtIO GPU",
            "init started: BusyBox v1.37.0",
            "desktop: mode",
            "desktop: client connected",
            "terminal: connected",
            "desktop: surface",
            "terminal: shell spawned",
        ),
        interactive_devices=True,
    )


def main() -> int:
    target = target_from_environment()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--image",
        type=Path,
        default=default_image(target),
        help="只读基准 rootfs；gate 不修改该镜像",
    )
    args = parser.parse_args()
    image = args.image.resolve()
    try:
        if not image.is_file():
            raise RuntimeError(f"rootfs image is missing: {image}")
        compiler = find_compiler()
        musl = cached_musl_paths(compiler)
        busybox = cached_busybox_binary(compiler)
        stamp = ROOT / "target" / "verify-gates" / f"boot-{target.arch}.json"
        payload = runtime_gate_payload(
            "boot-topology",
            4,
            gate_inputs(target, image, busybox, musl.install),
        )
        if runtime_gate_hit(stamp, payload, (image,)):
            print("QEMU boot verification cache hit")
            return 0
        for smp in SMP_CONFIGURATIONS:
            boot(image, smp)
            print(f"QEMU -smp {smp} boot verification passed")
        if target.arch == "aarch64":
            boot_interactive_devices(image)
            print("QEMU AArch64 interactive-device boot verification passed")
        publish_runtime_gate(stamp, payload)
    except RuntimeError as error:
        print(f"boot verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
