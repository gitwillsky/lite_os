from __future__ import annotations

import importlib
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
ROOT = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))

import verify_rust_std  # noqa: E402


def reload_rust_std(arch: str, accel: str) -> object:
    with patch.dict(os.environ, {"ARCH": arch, "ACCEL": accel}, clear=True):
        return importlib.reload(verify_rust_std)


class RustStdRoutingTests(unittest.TestCase):
    def test_standard_linux_musl_target_is_architecture_scoped(self) -> None:
        aarch64 = reload_rust_std("aarch64", "hvf")
        self.assertEqual(aarch64.RUST_USER_TARGET, "aarch64-unknown-linux-musl")
        self.assertEqual(aarch64.WORK.name, "aarch64")

        riscv64 = reload_rust_std("riscv64", "tcg")
        self.assertEqual(riscv64.RUST_USER_TARGET, "riscv64gc-unknown-linux-musl")
        self.assertEqual(riscv64.WORK.name, "riscv64")

    def test_fixture_installer_only_targets_disposable_binary_path(self) -> None:
        module = reload_rust_std("aarch64", "hvf")
        with tempfile.TemporaryDirectory() as workspace:
            directory = Path(workspace)
            image = directory / "fs.img"
            binary = directory / "rust-std-smoke"
            image.touch()
            binary.touch()
            with (
                patch.object(module, "find_debugfs", return_value=Path("/tool/debugfs")),
                patch.object(module, "run") as run,
            ):
                module.install_std_smoke(image, binary, directory)

            recipe = (directory / "rust-std-smoke.debugfs").read_text()
            self.assertIn(f"write {binary} /bin/rust-std-smoke", recipe)
            self.assertIn("set_inode_field /bin/rust-std-smoke mode 0100755", recipe)
            self.assertEqual(run.call_args.args[0][0], "/tool/debugfs")

    def test_rust_source_probe_drops_make_jobserver_capability(self) -> None:
        module = reload_rust_std("aarch64", "hvf")
        with tempfile.TemporaryDirectory() as workspace:
            sysroot = Path(workspace)
            manifest = sysroot / "lib/rustlib/src/rust/library/std/Cargo.toml"
            manifest.parent.mkdir(parents=True)
            manifest.touch()
            clean_environment = {"LC_ALL": "C"}
            with (
                patch.object(
                    module,
                    "rustc_probe_environment",
                    return_value=clean_environment,
                ),
                patch.object(module, "run", return_value=f"{sysroot}\n") as run,
            ):
                source = module.rust_source_root("/tool/rustc")

        self.assertEqual(source, sysroot / "lib/rustlib/src/rust")
        self.assertIs(run.call_args.args[2], clean_environment)

    def test_runtime_gate_binds_host_ipv4_fixture(self) -> None:
        module = reload_rust_std("aarch64", "hvf")
        musl = type("Musl", (), {"install": Path("/musl")})()
        inputs = module.gate_inputs(Path("/rootfs.img"), Path("/std"), musl)

        self.assertIn(module.ROOT / "user/base/udhcpc.script", inputs)
        self.assertIn(module.ROOT / module.TARGET.kernel_elf(), inputs)

    def test_make_routes_both_architectures_through_std_runtime_gate(self) -> None:
        makefile = (ROOT / "Makefile").read_text()

        self.assertIn("$(MAKE) verify-runtime-rust-std", makefile)
        self.assertIn(
            "ARCH=riscv64 ACCEL=tcg python3 scripts/verify_rust_std.py",
            makefile,
        )


if __name__ == "__main__":
    unittest.main()
