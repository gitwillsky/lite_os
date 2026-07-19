from __future__ import annotations

import os
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from build_target import (  # noqa: E402
    acceleration_from_environment,
    make_variables,
    target_from_environment,
)


class BuildTargetTests(unittest.TestCase):
    def test_defaults_select_aarch64_hvf(self) -> None:
        with patch.dict(os.environ, {}, clear=True):
            target = target_from_environment()
            acceleration = acceleration_from_environment()

        self.assertEqual(target.arch, "aarch64")
        self.assertEqual(target.kernel_triple, "aarch64-unknown-none-softfloat")
        self.assertEqual(target.linux_triple, "aarch64-linux-musl")
        self.assertEqual(target.qemu_binary, "qemu-system-aarch64")
        self.assertEqual(target.qemu_cpu(acceleration), "host")
        self.assertEqual(
            target.qemu_machine(acceleration),
            "virt,accel=hvf,gic-version=3,its=off,secure=off,"
            "virtualization=off,acpi=off",
        )
        self.assertEqual(target.musl_loader, "/lib/ld-musl-aarch64.so.1")
        self.assertEqual(target.alpine_arch, "aarch64")
        self.assertFalse(target.requires_bootloader)
        self.assertEqual(target.target_key, "aarch64-unknown-none-softfloat")
        self.assertEqual(
            target.kernel_elf(),
            "target/aarch64-unknown-none-softfloat/release/kernel",
        )
        self.assertEqual(
            target.kernel_boot_artifact(),
            "target/aarch64-unknown-none-softfloat/release/Image",
        )
        self.assertTrue(target.requires_raw_kernel_image)

    def test_aarch64_tcg_uses_max_cpu(self) -> None:
        environment = {"ARCH": "aarch64", "ACCEL": "tcg"}
        target = target_from_environment(environment)
        acceleration = acceleration_from_environment(environment)

        self.assertEqual(target.qemu_cpu(acceleration), "max")
        self.assertEqual(
            target.qemu_machine(acceleration),
            "virt,accel=tcg,gic-version=3,its=off,secure=off,"
            "virtualization=off,acpi=off",
        )

    def test_riscv64_mapping_and_tcg_cpu(self) -> None:
        environment = {"ARCH": "riscv64", "ACCEL": "tcg"}
        target = target_from_environment(environment)
        acceleration = acceleration_from_environment(environment)

        self.assertEqual(target.kernel_triple, "riscv64gc-unknown-none-elf")
        self.assertEqual(target.linux_triple, "riscv64-linux-musl")
        self.assertEqual(target.qemu_binary, "qemu-system-riscv64")
        self.assertEqual(target.qemu_cpu(acceleration), "rv64")
        self.assertEqual(target.qemu_machine(acceleration), "virt,accel=tcg")
        self.assertEqual(target.musl_loader, "/lib/ld-musl-riscv64.so.1")
        self.assertEqual(target.alpine_arch, "riscv64")
        self.assertTrue(target.requires_bootloader)
        self.assertEqual(target.target_key, "riscv64gc-unknown-none-elf")
        self.assertEqual(target.kernel_boot_artifact(), target.kernel_elf())
        self.assertFalse(target.requires_raw_kernel_image)

    def test_riscv64_rejects_hvf(self) -> None:
        target = target_from_environment({"ARCH": "riscv64"})
        acceleration = acceleration_from_environment({})

        with self.assertRaisesRegex(
            ValueError,
            "ACCEL=hvf is not supported for ARCH=riscv64",
        ):
            target.qemu_cpu(acceleration)

    def test_invalid_or_empty_arch_is_rejected(self) -> None:
        for value in ("arm64", "", " aarch64"):
            with self.subTest(value=value):
                with self.assertRaisesRegex(ValueError, "ARCH must be one of"):
                    target_from_environment({"ARCH": value})

    def test_invalid_or_empty_acceleration_is_rejected(self) -> None:
        for value in ("kvm", "", " hvf"):
            with self.subTest(value=value):
                with self.assertRaisesRegex(ValueError, "ACCEL must be one of"):
                    acceleration_from_environment({"ACCEL": value})

    def test_make_variables_validate_the_complete_combination(self) -> None:
        variables = make_variables({"ARCH": "aarch64", "ACCEL": "hvf"})

        self.assertEqual(variables["KERNEL_TARGET"], "aarch64-unknown-none-softfloat")
        self.assertEqual(variables["QEMU"], "qemu-system-aarch64")
        self.assertEqual(variables["QEMU_CPU"], "host")
        self.assertEqual(
            variables["QEMU_MACHINE"],
            "virt,accel=hvf,gic-version=3,its=off,secure=off,"
            "virtualization=off,acpi=off",
        )
        self.assertEqual(variables["BOOTLOADER_REQUIRED"], "0")
        self.assertEqual(variables["KERNEL_BOOT_NAME"], "Image")
        self.assertEqual(variables["RAW_KERNEL_IMAGE_REQUIRED"], "1")

        with self.assertRaisesRegex(ValueError, "ACCEL=hvf is not supported"):
            make_variables({"ARCH": "riscv64", "ACCEL": "hvf"})


if __name__ == "__main__":
    unittest.main()
