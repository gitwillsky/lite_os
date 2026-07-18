#!/usr/bin/env python3
"""Run deterministic non-test QEMU cold boots against several DTB hart sets."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from build_cache import publish_runtime_gate, runtime_gate_hit, runtime_gate_payload
from qemu_gate import boot as boot_image
from verify_busybox import cached_busybox_binary
from verify_musl import cached_musl_paths, find_compiler

ROOT = Path(__file__).resolve().parent.parent
# musl 和 BusyBox runtime gates 已分别覆盖 1/8 hart；这里只保留独有的非幂次 3-hart DTB。
SMP_CONFIGURATIONS = (3,)


def boot(image: Path, smp: int) -> None:
    """冷启动指定 rootfs，并核对动态 hart topology 与 BusyBox init。

    Args:
        image: 只读基准 rootfs；QEMU helper 会创建私有可写副本。
        smp: 本次 QEMU 向 guest 暴露的 hart 数。

    Returns:
        None；全部启动标记出现后返回。

    Raises:
        RuntimeError: QEMU 启动失败、超时或缺少预期标记。
    """
    expected_mask = (1 << smp) - 1
    markers = (
        f"dynamic hart topology initialized: count={smp}, mask={expected_mask:#x}",
        f"all DTB harts online: count={smp}, mask={expected_mask:#x}",
        "init started: BusyBox v1.37.0",
    )
    boot_image(image, smp, markers)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--image",
        type=Path,
        default=ROOT / "target" / "rootfs.img",
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
        stamp = ROOT / "target/verify-gates/boot.json"
        payload = runtime_gate_payload(
            "boot-topology",
            3,
            (
                image,
                ROOT / "target/riscv64gc-unknown-none-elf/debug/kernel",
                ROOT / "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
                busybox,
                musl.install / "usr/lib/libc.so",
                ROOT / "user/base/inittab",
                ROOT / "create_fs.py",
                ROOT / "scripts/verify_busybox.py",
                Path(__file__).resolve(),
                ROOT / "scripts/qemu_gate.py",
            ),
        )
        if runtime_gate_hit(stamp, payload, (image,)):
            print("QEMU boot verification cache hit")
            return 0
        for smp in SMP_CONFIGURATIONS:
            boot(image, smp)
            print(f"QEMU -smp {smp} boot verification passed")
        publish_runtime_gate(stamp, payload)
    except RuntimeError as error:
        print(f"boot verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
