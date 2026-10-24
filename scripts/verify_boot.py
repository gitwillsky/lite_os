#!/usr/bin/env python3
"""Run deterministic non-test QEMU cold boots against several DTB hart sets."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from build_cache import publish_runtime_gate, runtime_gate_hit, runtime_gate_payload
from build_target import BuildTarget, acceleration_from_environment, target_from_environment
from qemu_gate import boot as boot_image, cpu_topology_markers
from utm_gate import boot_graphics
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
        ROOT / "user/base/rcS",
        ROOT / "create_fs.py",
        ROOT / "scripts/verify_busybox.py",
        Path(__file__).resolve(),
        ROOT / "scripts/qemu_gate.py",
        ROOT / "scripts/utm_gate.py",
        ROOT / "scripts/utm_runtime.py",
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


def boot_product_graphics(image: Path, kernel: Path) -> None:
    """通过隐藏 UTM VirGL 产品路径验证 GPU、输入设备拓扑与桌面全链路。

    compositor 首帧 marker 证明真实 splash 已由 Aurora 桌面原子替换；随后
    Files 与 Terminal 的 marker 证明两个固定启动窗口完成首帧发布。
    """
    boot_graphics(
        image,
        kernel,
        (
            "VirtIO Sound capability ready",
            "VirtIO console PCI transport on IRQ",
            "VirtIO input event0",
            "VirtIO input event1",
            "VirtIO GPU",
            "init started: BusyBox v1.37.0",
            "compositor: GPU mode",
            "compositor: desktop connected",
            "compositor: desktop first scene presented",
            "lite-ui: desktop ready",
            "compositor: app 1 connected",
            "lite-ui: app terminal ready",
            "terminal-session: shell spawned",
            "compositor: app 2 connected",
            "lite-ui: app file-manager ready",
        ),
        kernel_arguments="liteos.target=desktop",
    )


def boot_product_seat_switch(image: Path, kernel: Path) -> None:
    """默认文本控制台启动，经 seatctl 切到桌面再切回，验证座位交接的两个方向。

    串口 marker 证明：文本控制台完成首帧；切到桌面时控制台先释放、compositor 随后取得座位并呈现
    桌面首帧；切回时 compositor 先释放、控制台重新取得座位。缺少先释放后取得的顺序，后一个
    session 的 `set_master` 会因 EBUSY 失败，对应 marker 永远不会出现。
    """
    boot_graphics(
        image,
        kernel,
        (
            "VirtIO GPU",
            "seatd: ready target=text",
            "seatd: active text",
            "console: acquired 1",
            "console: first frame presented",
            "console: shell spawned",
            "compositor: GPU mode",
            "compositor: seat registered",
            "console: released 1",
            "compositor: seat acquired 1",
            "seatd: active desktop",
            "compositor: desktop first scene presented",
            "compositor: seat released 1",
            "console: acquired 2",
        ),
        interactions=(
            # 触发点必须与门禁要验证的前提一致：文本控制台已持有座位并呈现首帧。若以 compositor 就绪为触发，
            # compositor 初始化变快（它与 console 是互不排序的两个进程）就会让命令抢在 console 取得座位之前发出，
            # seatd 把座位从尚未持有它的 console 切走，`console: released 1` 永远不会出现。
            ("console: first frame presented", b"seatctl desktop\n"),
            ("compositor: desktop first scene presented", b"seatctl text\n"),
        ),
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
            8,
            gate_inputs(target, image, busybox, musl.install),
        )
        if runtime_gate_hit(stamp, payload, (image,)):
            print("QEMU boot verification cache hit")
            return 0
        for smp in SMP_CONFIGURATIONS:
            boot(image, smp)
            print(f"QEMU -smp {smp} boot verification passed")
        if (
            target.arch == "aarch64"
            and acceleration_from_environment() == "hvf"
        ):
            boot_product_graphics(image, ROOT / target.kernel_boot_artifact())
            print("UTM AArch64 VirGL product graphics boot verification passed")
            boot_product_seat_switch(image, ROOT / target.kernel_boot_artifact())
            print("UTM AArch64 text console and desktop seat switch verification passed")
        elif target.arch == "aarch64":
            print("UTM VirGL boot gate skipped outside the AArch64/HVF product runtime")
        publish_runtime_gate(stamp, payload)
    except RuntimeError as error:
        print(f"boot verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
