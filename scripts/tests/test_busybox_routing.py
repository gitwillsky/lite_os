from __future__ import annotations

import importlib
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import verify_busybox  # noqa: E402


def reload_busybox(arch: str, accel: str) -> object:
    with patch.dict(os.environ, {"ARCH": arch, "ACCEL": accel}, clear=True):
        return importlib.reload(verify_busybox)


class BusyBoxRoutingTests(unittest.TestCase):
    def test_aarch64_build_and_runtime_identity(self) -> None:
        module = reload_busybox("aarch64", "hvf")

        self.assertEqual(module.WORK.name, "aarch64")
        self.assertEqual(module.BUSYBOX_ARCH, "arm64")
        self.assertEqual(module.BUSYBOX_TARGET_CFLAGS, "-march=armv8-a")
        self.assertEqual(module.ELF_MACHINE, "AArch64")
        self.assertEqual(module.RUST_USER_TARGET, "aarch64-unknown-linux-musl")
        self.assertIn("ARCH=aarch64", module.BUSYBOX_CC)
        self.assertIn("ARCH=aarch64", module.BUSYBOX_LD)
        self.assertNotIn("ARCH=arm64", module.BUSYBOX_CC)
        self.assertEqual(module.TARGET.linux_triple, "aarch64-linux-musl")
        self.assertEqual(module.TARGET.musl_loader, "/lib/ld-musl-aarch64.so.1")
        self.assertEqual(
            module.target_runtime_artifacts(),
            (
                module.ROOT
                / "target/aarch64-unknown-none-softfloat/release/kernel",
            ),
        )

    def test_riscv64_route_preserves_bootloader_and_build_arch(self) -> None:
        module = reload_busybox("riscv64", "tcg")

        self.assertEqual(module.WORK.name, "riscv64")
        self.assertEqual(module.BUSYBOX_ARCH, "riscv")
        self.assertEqual(module.BUSYBOX_TARGET_CFLAGS, "-march=rv64gc -mabi=lp64d")
        self.assertEqual(module.ELF_MACHINE, "RISC-V")
        self.assertEqual(module.RUST_USER_TARGET, "riscv64gc-unknown-linux-musl")
        self.assertIn("ARCH=riscv64", module.BUSYBOX_CC)
        self.assertIn("ARCH=riscv64", module.BUSYBOX_LD)
        self.assertNotIn("ARCH=riscv ", module.BUSYBOX_CC)
        self.assertEqual(
            module.target_runtime_artifacts(),
            (
                module.ROOT / "target/riscv64gc-unknown-none-elf/release/kernel",
                module.ROOT
                / "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
            ),
        )

    def test_missing_aarch64_rust_target_is_explicit_blocker(self) -> None:
        module = reload_busybox("aarch64", "hvf")
        musl = Mock()
        with (
            patch.object(module.shutil, "which", side_effect=["/tool/cargo", "/tool/rustc"]),
            patch.object(
                module,
                "run",
                return_value="riscv64gc-unknown-linux-musl\n",
            ),
        ):
            with self.assertRaisesRegex(
                RuntimeError,
                "refusing to reuse another architecture",
            ):
                module.build_rust_user_program(
                    musl,
                    "console-session",
                    "console-session",
                    "console-session",
                    1,
                )

    def test_verify_elf_accepts_aarch64_machine_and_loader(self) -> None:
        module = reload_busybox("aarch64", "hvf")
        output = "\n".join(
            (
                "ELF64 AArch64 DYN (Position-Independent Executable file)",
                "INTERP Requesting program interpreter: /lib/ld-musl-aarch64.so.1",
                "DYNAMIC GNU_RELRO NOW PIE",
                "NEEDED Shared library: [libc.so]",
                "LOAD 0x0 0x0 0x0 0x100 0x100 R E 0x1000",
                "GNU_STACK 0x0 0x0 0x0 0x0 0x0 RW 0x10",
            )
        )
        with tempfile.TemporaryDirectory() as directory:
            compiler = Path(directory) / "aarch64-none-elf-gcc"
            readelf = Path(directory) / "aarch64-none-elf-readelf"
            binary = Path(directory) / "busybox"
            for path in (compiler, readelf, binary):
                path.touch()
            with patch.object(module, "run", return_value=output):
                module.verify_elf(binary, compiler)


if __name__ == "__main__":
    unittest.main()
