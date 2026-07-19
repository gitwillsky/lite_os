from __future__ import annotations

import sys
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import verify_boot  # noqa: E402
from build_target import target_from_environment  # noqa: E402


class VerifyBootRoutingTests(unittest.TestCase):
    def test_aarch64_uses_release_kernel_without_bootloader(self) -> None:
        target = target_from_environment({"ARCH": "aarch64"})
        image = Path("/images/aarch64.img")

        inputs = verify_boot.gate_inputs(
            target,
            image,
            Path("/cache/busybox"),
            Path("/cache/musl"),
        )

        self.assertEqual(verify_boot.default_image(target).name, "aarch64.img")
        self.assertIn(image, inputs)
        self.assertIn(
            verify_boot.ROOT
            / "target/aarch64-unknown-none-softfloat/release/kernel",
            inputs,
        )
        self.assertIn(
            verify_boot.ROOT
            / "target/aarch64-unknown-none-softfloat/release/Image",
            inputs,
        )
        self.assertFalse(any("bootloader/target" in str(path) for path in inputs))

    def test_riscv64_uses_its_release_kernel_and_bootloader(self) -> None:
        target = target_from_environment({"ARCH": "riscv64"})

        inputs = verify_boot.gate_inputs(
            target,
            Path("/images/riscv64.img"),
            Path("/cache/busybox"),
            Path("/cache/musl"),
        )

        self.assertEqual(verify_boot.default_image(target).name, "riscv64.img")
        self.assertIn(
            verify_boot.ROOT / "target/riscv64gc-unknown-none-elf/release/kernel",
            inputs,
        )
        self.assertIn(
            verify_boot.ROOT
            / "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
            inputs,
        )


if __name__ == "__main__":
    unittest.main()
