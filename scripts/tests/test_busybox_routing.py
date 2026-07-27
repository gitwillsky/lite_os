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

    def test_login_path_exposes_npm_global_commands(self) -> None:
        module = reload_busybox("aarch64", "hvf")
        profile = (module.ROOT / "user/base/profile").read_text().strip()

        self.assertEqual(
            profile,
            "export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
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
                    "desktop",
                    "desktop",
                    "desktop",
                    1,
                )

    def test_rust_user_cargo_cache_identity_excludes_source_revision(self) -> None:
        module = reload_busybox("aarch64", "hvf")
        musl = Mock(sysroot_fingerprint="sysroot-a")
        with tempfile.TemporaryDirectory() as directory, patch.object(
            module, "WORK", Path(directory)
        ):
            first = module.rust_user_cargo_target(
                musl,
                "cargo 1",
                "rustc 1",
                "driver-a",
                "unwind-a",
            )
            second = module.rust_user_cargo_target(
                musl,
                "cargo 1",
                "rustc 1",
                "driver-a",
                "unwind-a",
            )
            changed_sysroot = module.rust_user_cargo_target(
                Mock(sysroot_fingerprint="sysroot-b"),
                "cargo 1",
                "rustc 1",
                "driver-a",
                "unwind-a",
            )

        self.assertEqual(first, second)
        self.assertNotEqual(first, changed_sysroot)
        self.assertEqual(first.parent.name, "rust-user-cargo-targets")

    def test_ui_asset_payload_ignores_outputs_and_tracks_external_assets(self) -> None:
        module = reload_busybox("aarch64", "hvf")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            ui = root / "ui"
            source = ui / "src/main.ts"
            ignored_dependency = ui / "node_modules/package/index.js"
            ignored_output = ui / "dist/main.js"
            external = root / "assets/icon.png"
            for path, content in (
                (source, "source-a"),
                (ignored_dependency, "dependency-a"),
                (ignored_output, "output-a"),
                (external, "asset-a"),
            ):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content)

            with (
                patch.object(module, "ROOT", root),
                patch.object(module, "UI_EXTERNAL_INPUTS", (external,)),
            ):
                initial = module.ui_asset_payload(ui, "node 1", "npm 1")
                ignored_dependency.write_text("dependency-b")
                ignored_output.write_text("output-b")
                ignored = module.ui_asset_payload(ui, "node 1", "npm 1")
                external.write_text("asset-b")
                changed = module.ui_asset_payload(ui, "node 1", "npm 1")

        self.assertEqual(initial, ignored)
        self.assertNotEqual(ignored, changed)

    def test_ui_asset_cache_hit_skips_npm_build(self) -> None:
        module = reload_busybox("aarch64", "hvf")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            work = root / "target"
            ui = root / "ui"
            source = ui / "src/main.ts"
            external = root / "assets/icon.png"
            for path in (source, external):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(path.name)

            def version_output(command: list[str], _: Path) -> str:
                if command == ["/tools/node", "--version"]:
                    return "node 1"
                if command == ["/tools/npm", "--version"]:
                    return "npm 1"
                raise AssertionError(f"unexpected build command: {command}")

            with (
                patch.object(module, "ROOT", root),
                patch.object(module, "WORK", work),
                patch.object(module, "UI_EXTERNAL_INPUTS", (external,)),
                patch.object(
                    module.shutil,
                    "which",
                    side_effect=lambda name: f"/tools/{name}",
                ),
                patch.object(module, "run", side_effect=version_output) as run_mock,
            ):
                payload = module.ui_asset_payload(ui, "node 1", "npm 1")
                entry = work / "ui-assets" / module.fingerprint(payload)
                for relative in module.UI_REQUIRED_OUTPUTS:
                    output = entry / "output" / relative
                    output.parent.mkdir(parents=True, exist_ok=True)
                    output.write_text(relative)
                module.write_manifest(entry, payload)

                observed = module.build_ui_assets()

        self.assertEqual(observed, entry / "output")
        self.assertEqual(run_mock.call_count, 2)

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
