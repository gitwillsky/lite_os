#!/usr/bin/env python3
"""集中描述 LiteOS 构建目标与 QEMU 加速组合。"""

from __future__ import annotations

import argparse
import os
from dataclasses import dataclass
from typing import Literal, Mapping

Architecture = Literal["aarch64", "riscv64"]
Acceleration = Literal["hvf", "tcg"]

SUPPORTED_ARCHITECTURES: tuple[Architecture, ...] = ("aarch64", "riscv64")
SUPPORTED_ACCELERATIONS: tuple[Acceleration, ...] = ("hvf", "tcg")


@dataclass(frozen=True)
class BuildTarget:
    """一个架构的构建、用户态与 QEMU 静态映射。"""

    arch: Architecture
    kernel_triple: str
    linux_triple: str
    qemu_binary: str
    musl_loader: str
    alpine_arch: str
    requires_bootloader: bool
    target_key: str
    kernel_boot_name: str

    def kernel_elf(self, profile: str = "release") -> str:
        """返回保留符号与 ELF metadata 的 kernel 调试产物路径。"""
        return f"target/{self.kernel_triple}/{profile}/kernel"

    def kernel_boot_artifact(self, profile: str = "release") -> str:
        """返回 QEMU ``-kernel`` 必须消费的架构启动产物路径。"""
        return f"target/{self.kernel_triple}/{profile}/{self.kernel_boot_name}"

    @property
    def requires_raw_kernel_image(self) -> bool:
        """返回该架构是否要求由 ELF 派生 raw Linux Image。"""
        return self.kernel_boot_name != "kernel"

    def qemu_cpu(self, acceleration: Acceleration) -> str:
        """返回该目标与加速器组合所需的 QEMU CPU model。

        Raises:
            ValueError: 加速器值未知，或架构不支持该加速器。
        """
        checked = _parse_acceleration(acceleration)
        if self.arch == "aarch64":
            return "host" if checked == "hvf" else "max"
        if checked == "hvf":
            raise ValueError(
                "ACCEL=hvf is not supported for ARCH=riscv64; use ACCEL=tcg"
            )
        return "rv64"

    def qemu_machine(self, acceleration: Acceleration) -> str:
        """返回完整且已验证的 QEMU machine/acceleration 配置。

        Raises:
            ValueError: 加速器值未知，或架构不支持该加速器。
        """
        checked = _parse_acceleration(acceleration)
        self.qemu_cpu(checked)
        if self.arch == "aarch64":
            return (
                f"virt,accel={checked},gic-version=3,its=off,secure=off,"
                "virtualization=off,acpi=off"
            )
        return "virt,accel=tcg"


_TARGETS: dict[Architecture, BuildTarget] = {
    "aarch64": BuildTarget(
        arch="aarch64",
        kernel_triple="aarch64-unknown-none-softfloat",
        linux_triple="aarch64-linux-musl",
        qemu_binary="qemu-system-aarch64",
        musl_loader="/lib/ld-musl-aarch64.so.1",
        alpine_arch="aarch64",
        requires_bootloader=False,
        target_key="aarch64-unknown-none-softfloat",
        kernel_boot_name="Image",
    ),
    "riscv64": BuildTarget(
        arch="riscv64",
        kernel_triple="riscv64gc-unknown-none-elf",
        linux_triple="riscv64-linux-musl",
        qemu_binary="qemu-system-riscv64",
        musl_loader="/lib/ld-musl-riscv64.so.1",
        alpine_arch="riscv64",
        requires_bootloader=True,
        target_key="riscv64gc-unknown-none-elf",
        kernel_boot_name="kernel",
    ),
}


def _parse_architecture(value: str) -> Architecture:
    if value == "aarch64" or value == "riscv64":
        return value
    allowed = ", ".join(SUPPORTED_ARCHITECTURES)
    raise ValueError(f"ARCH must be one of: {allowed}; got {value!r}")


def _parse_acceleration(value: str) -> Acceleration:
    if value == "hvf" or value == "tcg":
        return value
    allowed = ", ".join(SUPPORTED_ACCELERATIONS)
    raise ValueError(f"ACCEL must be one of: {allowed}; got {value!r}")


def target_from_environment(
    environment: Mapping[str, str] | None = None,
) -> BuildTarget:
    """从 ``ARCH`` 读取目标；变量缺失时选择 first-class AArch64。"""
    source = os.environ if environment is None else environment
    arch = _parse_architecture(source.get("ARCH", "aarch64"))
    return _TARGETS[arch]


def acceleration_from_environment(
    environment: Mapping[str, str] | None = None,
) -> Acceleration:
    """从 ``ACCEL`` 读取 QEMU 加速器；变量缺失时选择 HVF。"""
    source = os.environ if environment is None else environment
    return _parse_acceleration(source.get("ACCEL", "hvf"))


def make_variables(
    environment: Mapping[str, str] | None = None,
) -> dict[str, str]:
    """返回 Make 与 Python gate 共用的已验证 target 映射。

    Args:
        environment: 包含可选 ``ARCH``/``ACCEL`` 的环境；缺省读取进程环境。

    Returns:
        只包含静态白名单值的 Make 变量字典。

    Raises:
        ValueError: 架构、加速器未知，或组合不受支持。缺少验证会让 Make
        构建一个 target、QEMU 却启动另一个 target 的旧产物。
    """
    target = target_from_environment(environment)
    acceleration = acceleration_from_environment(environment)
    return {
        "ARCH": target.arch,
        "ACCEL": acceleration,
        "KERNEL_TARGET": target.kernel_triple,
        "LINUX_TARGET": target.linux_triple,
        "QEMU": target.qemu_binary,
        "QEMU_CPU": target.qemu_cpu(acceleration),
        "QEMU_MACHINE": target.qemu_machine(acceleration),
        "MUSL_LOADER": target.musl_loader,
        "ALPINE_ARCH": target.alpine_arch,
        "BOOTLOADER_REQUIRED": "1" if target.requires_bootloader else "0",
        "KERNEL_BOOT_NAME": target.kernel_boot_name,
        "RAW_KERNEL_IMAGE_REQUIRED": "1" if target.requires_raw_kernel_image else "0",
        "TARGET_KEY": target.target_key,
    }


def main() -> int:
    """输出一个已验证字段，供 Make 在解析阶段消费。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--field", choices=tuple(make_variables()), required=True)
    arguments = parser.parse_args()
    print(make_variables()[arguments.field])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
