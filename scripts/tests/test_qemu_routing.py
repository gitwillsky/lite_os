from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import build_cache  # noqa: E402
import qemu_gate  # noqa: E402


def argument_after(command: list[str], option: str) -> str:
    return command[command.index(option) + 1]


class QemuRoutingTests(unittest.TestCase):
    def test_fatal_line_drain_collects_the_rest_of_the_current_line(self) -> None:
        read_fd, write_fd = os.pipe()
        try:
            with os.fdopen(read_fd, "rb", closefd=False) as stream:
                os.write(write_fd, b"l] access fault\nnext line")
                output = bytearray(b"[CPU-0] [ERROR] [kerne")
                qemu_gate.drain_fatal_line(stream, output)
        finally:
            os.close(read_fd)
            os.close(write_fd)

        self.assertEqual(output, b"[CPU-0] [ERROR] [kernel] access fault\nnext line")

    @patch("qemu_gate.shutil.which", return_value="/opt/qemu-system-aarch64")
    def test_aarch64_hvf_direct_kernel_route(self, _: Mock) -> None:
        with patch.dict(os.environ, {}, clear=True):
            command = qemu_gate._qemu_command(Path("rootfs.img"), 7)

        self.assertEqual(command[0], "/opt/qemu-system-aarch64")
        self.assertEqual(
            argument_after(command, "-machine"),
            "virt,accel=hvf,gic-version=3,its=off,secure=off,"
            "virtualization=off,acpi=off",
        )
        self.assertEqual(argument_after(command, "-cpu"), "host")
        self.assertNotIn("-accel", command)
        self.assertNotIn("-acpi", command)
        self.assertEqual(argument_after(command, "-smp"), "7")
        self.assertEqual(
            argument_after(command, "-kernel"),
            "target/aarch64-unknown-none-softfloat/release/Image",
        )
        self.assertNotIn("-bios", command)

    @patch("qemu_gate.shutil.which", return_value="/opt/qemu-system-aarch64")
    def test_aarch64_tcg_uses_max_cpu(self, _: Mock) -> None:
        with patch.dict(os.environ, {"ARCH": "aarch64", "ACCEL": "tcg"}, clear=True):
            command = qemu_gate._qemu_command(Path("rootfs.img"), 1)

        self.assertIn("accel=tcg", argument_after(command, "-machine"))
        self.assertEqual(argument_after(command, "-cpu"), "max")

    @patch("qemu_gate.shutil.which", return_value="/opt/qemu-system-aarch64")
    def test_aarch64_interactive_devices_match_run_gui_topology(self, _: Mock) -> None:
        with patch.dict(os.environ, {}, clear=True):
            command = qemu_gate._qemu_command(
                Path("rootfs.img"), 11, interactive_devices=True
            )

        devices = [
            command[index + 1]
            for index, argument in enumerate(command)
            if argument == "-device"
        ]
        self.assertEqual(
            devices,
            [
                "virtio-blk-device,drive=x0",
                "virtio-rng-device,rng=rng0",
                "virtio-gpu-device,xres=3008,yres=1692",
                "virtio-keyboard-device",
                "virtio-tablet-device",
                "virtio-net-device,netdev=net0",
            ],
        )

    @patch("qemu_gate.shutil.which", return_value="/opt/qemu-system-riscv64")
    def test_riscv64_tcg_keeps_rustsbi_and_release_kernel(self, _: Mock) -> None:
        environment = {"ARCH": "riscv64", "ACCEL": "tcg"}
        with patch.dict(os.environ, environment, clear=True):
            command = qemu_gate._qemu_command(Path("rootfs.img"), 2)

        self.assertEqual(command[0], "/opt/qemu-system-riscv64")
        self.assertEqual(argument_after(command, "-machine"), "virt,accel=tcg")
        self.assertEqual(argument_after(command, "-cpu"), "rv64")
        self.assertEqual(
            argument_after(command, "-bios"),
            "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
        )
        self.assertEqual(
            argument_after(command, "-kernel"),
            "target/riscv64gc-unknown-none-elf/release/kernel",
        )
        self.assertNotIn("-acpi", command)

    def test_riscv64_hvf_fails_before_process_launch(self) -> None:
        with patch.dict(os.environ, {"ARCH": "riscv64"}, clear=True):
            with self.assertRaisesRegex(ValueError, "ACCEL=hvf is not supported"):
                qemu_gate._qemu_command(Path("rootfs.img"), 1)

    def test_runtime_payload_contains_complete_qemu_identity(self) -> None:
        version = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="QEMU emulator version 11.0.1\n",
        )
        with tempfile.TemporaryDirectory() as directory:
            input_path = Path(directory) / "kernel"
            input_path.write_bytes(b"kernel")
            environment = {"ARCH": "aarch64", "ACCEL": "tcg"}
            with (
                patch.dict(os.environ, environment, clear=True),
                patch(
                    "build_cache.shutil.which",
                    return_value="/opt/qemu-system-aarch64",
                ),
                patch("build_cache.subprocess.run", return_value=version),
            ):
                payload = build_cache.runtime_gate_payload("boot", 4, (input_path,))

        self.assertEqual(
            payload["qemu"],
            {
                "path": "/opt/qemu-system-aarch64",
                "version": "QEMU emulator version 11.0.1",
                "arch": "aarch64",
                "accel": "tcg",
                "cpu": "max",
                "machine": (
                    "virt,accel=tcg,gic-version=3,its=off,secure=off,"
                    "virtualization=off,acpi=off"
                ),
                "kernel_boot_artifact": (
                    "target/aarch64-unknown-none-softfloat/release/Image"
                ),
            },
        )

    @patch("qemu_gate.shutil.which", return_value=None)
    def test_missing_selected_qemu_reports_binary(self, _: Mock) -> None:
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(RuntimeError, "qemu-system-aarch64 is required"):
                qemu_gate._qemu_command(Path("rootfs.img"), 1)


if __name__ == "__main__":
    unittest.main()
