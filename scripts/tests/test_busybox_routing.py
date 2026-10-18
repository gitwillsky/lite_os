from __future__ import annotations

import importlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import verify_busybox  # noqa: E402
import openssl_cache  # noqa: E402
from build_cache import CACHE_MANIFEST  # noqa: E402


def reload_busybox(arch: str, accel: str) -> object:
    with patch.dict(os.environ, {"ARCH": arch, "ACCEL": accel}, clear=True):
        return importlib.reload(verify_busybox)


class BusyBoxRoutingTests(unittest.TestCase):
    def test_probe_binary_change_invalidates_runtime_success_before_cache_lookup(self) -> None:
        module = reload_busybox("aarch64", "tcg")
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            libc = work / "usr/lib/libc.so"
            libc.parent.mkdir(parents=True)
            libc.write_bytes(b"libc")
            names = ("memory-file", "special-file", "fs-stress", "inotify")
            probes = {name: work / f"{name}-probe" for name in names}
            for path in probes.values(): path.write_bytes(b"probe-v1")
            artifact = work / "kernel"
            artifact.write_bytes(b"kernel")
            payloads = []

            def hit(_stamp, payload, _artifacts):
                payloads.append(payload)
                return True

            with (
                patch.dict(os.environ, {"ARCH": "aarch64", "ACCEL": "tcg"}),
                patch.object(module.sys, "argv", ["verify_busybox.py"]),
                patch.object(module, "WORK", work),
                patch.object(module, "find_compiler", return_value=artifact),
                patch.object(module, "obtain_source", return_value=work),
                patch.object(module, "build_busybox", return_value=artifact),
                patch.object(module, "verify_elf"),
                patch.object(module, "cached_musl_paths", return_value=Mock(install=work)),
                patch.object(module, "build_openssl", return_value=Mock(binary=artifact)),
                patch.object(module, "build_dynamic_probe", return_value=(artifact, artifact)),
                patch.object(module, "build_musl_probe", side_effect=lambda _musl, name: probes[name]),
                patch.object(module, "target_runtime_artifacts", return_value=(artifact,)),
                patch.object(module, "runtime_gate_hit", side_effect=hit),
                patch("build_cache.validate_kernel_boot_artifact", return_value=artifact),
                patch("build_cache.shutil.which", return_value="/tmp/qemu"),
                patch("build_cache.subprocess.run", return_value=subprocess.CompletedProcess([], 0, "QEMU fixture\n")),
            ):
                self.assertEqual(module.main(), 0)
                probes["memory-file"].write_bytes(b"probe-v2")
                self.assertEqual(module.main(), 0)
            self.assertNotEqual(payloads[0]["inputs"], payloads[1]["inputs"])
            for path in probes.values(): self.assertIn(str(path), payloads[0]["inputs"])
            self.assertEqual(payloads[0]["qemu"], payloads[1]["qemu"])

    def test_openssl_strip_cache_identity_matches_executed_symlink(self) -> None:
        with patch.dict(os.environ, {"ARCH": "aarch64", "ACCEL": "tcg"}, clear=True):
            module = importlib.reload(openssl_cache)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            compiler = root / "clang"
            compiler.write_text("clang")
            executable = root / "llvm-objcopy"
            executable.write_text("strip-v1")
            executable.chmod(0o755)
            stripper = root / "llvm-strip"
            stripper.symlink_to(executable)
            ca = root / "ca.pem"
            ca.write_text("trust")
            musl = Mock(
                sysroot_fingerprint="musl", compiler=compiler,
                linker=root / "lld", compiler_runtime=root / "runtime.a",
                install=root / "sysroot", archiver=root / "llvm-ar",
            )

            def build(arguments: list[str], cwd: Path, _environment=None) -> None:
                if "build_programs" in arguments:
                    (cwd / "apps").mkdir()
                    (cwd / "apps/openssl").write_text("ELF")
                elif arguments[0] == str(stripper):
                    Path(arguments[2]).write_text("stripped ELF")

            with (
                patch.object(module, "WORK", root / "cache"),
                patch.object(module, "_source", return_value=(root / "source", "source")),
                patch.object(module, "_ca_bundle", return_value=ca),
                patch.object(module, "compiler_identity", return_value={}),
                patch.object(module.shutil, "which", return_value=None),
                patch.object(module, "run", side_effect=build) as run_mock,
            ):
                initial = module.build_openssl(musl, 1)
                executable.write_text("strip-v2")
                changed = module.build_openssl(musl, 1)
                self.assertNotEqual(initial.fingerprint, changed.fingerprint)
                manifest = json.loads((changed.binary.parent / CACHE_MANIFEST).read_text())
                # 身份与实际执行共用同一路径：只解析目录别名（macOS /var -> /private/var），
                # 文件名必须保持被执行的符号链接。
                executed = str(stripper.parent.resolve() / stripper.name)
                self.assertEqual(manifest["recipe"]["strip"]["path"], executed)
                self.assertEqual(run_mock.call_args.args[0][0], executed)

    def test_busybox_strip_identity_matches_executed_tool(self) -> None:
        module = reload_busybox("aarch64", "tcg")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            compiler = root / "clang"
            stripper = root / "llvm-strip"
            strip_executable = root / "llvm-objcopy"
            compiler.write_text("clang")
            strip_executable.write_text("strip-v1")
            strip_executable.chmod(0o755)
            stripper.symlink_to(strip_executable)
            config = root / "config"
            config.write_text("config")
            musl = Mock(
                sysroot_fingerprint="musl",
                compiler=compiler,
                linker=root / "lld",
                compiler_runtime=root / "runtime.a",
                install=root / "sysroot",
                archiver=root / "llvm-ar",
            )

            def configure(_source: Path, build: Path, _env: dict) -> None:
                (build / "busybox").write_text("ELF")
                (build / "busybox_unstripped").write_text("ELF with symbols")

            with (
                patch.object(module, "WORK", root / "cache"),
                patch.object(module, "CONFIG_FRAGMENT", config),
                patch.object(module, "source_payload", return_value={}),
                patch.object(module, "compiler_identity", return_value={}),
                patch.object(module.shutil, "which", return_value=None),
                patch.object(module, "cached_musl_paths", return_value=musl),
                patch.object(module, "configure", side_effect=configure),
                patch.object(module, "run") as run_mock,
            ):
                initial = module.binary_payload(compiler, musl)
                stripper.write_text("strip-v2")
                changed = module.binary_payload(compiler, musl)
                binary = module.build_busybox(root / "source", compiler, 1)
                cache = root / "cache/binaries" / module.fingerprint(changed)
                self.assertEqual(binary, (cache / "busybox").resolve())
                self.assertTrue(
                    module.manifest_matches(cache, changed, ("busybox", "busybox_unstripped"))
                )
                # 身份与实际执行共用同一路径：只解析目录别名（macOS /var -> /private/var）。
                executed = str(stripper.parent.resolve() / stripper.name)
                self.assertEqual(changed["strip"]["path"], executed)
                self.assertIn(f"STRIP={executed}", run_mock.call_args.args[0])
            self.assertNotEqual(module.fingerprint(initial), module.fingerprint(changed))

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

    def test_ui_lockfile_resolves_only_through_official_registry(self) -> None:
        # npm 只把官方 host 替换为 UI_NPM_REGISTRY；私有 registry URL 会绕过镜像并在其他 host 上失败。
        lockfile = json.loads((SCRIPTS.parent / "ui/package-lock.json").read_text())
        resolved = {
            entry["resolved"].split("/", 3)[2]
            for entry in lockfile["packages"].values()
            if "resolved" in entry
        }

        self.assertEqual(resolved, {"registry.npmjs.org"})

    def test_login_path_exposes_npm_global_commands(self) -> None:
        module = reload_busybox("aarch64", "hvf")
        profile = (module.ROOT / "user/base/profile").read_text().strip()
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(
                module, "build_ui_assets", return_value=Path(temporary)
            ),
            patch.object(
                module, "build_audio_service", return_value=Path("audio-service")
            ),
            patch.object(module, "build_compositor", return_value=Path("compositor")),
            patch.object(
                module, "build_app_binary", side_effect=lambda _musl, app_id: Path(app_id)
            ),
            patch.object(
                module, "build_session_launch", return_value=Path("session-launch")
            ),
            patch.object(
                module,
                "build_terminal_session",
                return_value=Path("terminal-session"),
            ),
        ):
            artifacts = module.build_graphical_userland(Mock())

        self.assertEqual(
            profile,
            "export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        self.assertIn(
            module.UserlandArtifact(module.ROOT / "user/base/profile", "/etc/profile"),
            artifacts,
        )

    def test_package_script_shell_registry_applets_are_enabled(self) -> None:
        module = reload_busybox("aarch64", "hvf")
        config = module.CONFIG_FRAGMENT.read_text().splitlines()

        self.assertIn("CONFIG_ADD_SHELL=y", config)
        self.assertIn("CONFIG_REMOVE_SHELL=y", config)

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


class MuslDevelopmentCommandsTests(unittest.TestCase):
    def test_installs_headers_startup_objects_and_archives_but_not_loader(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            install = Path(directory)
            (install / "usr/include/sys").mkdir(parents=True)
            (install / "usr/lib").mkdir(parents=True)
            (install / "usr/include/stdio.h").write_text("")
            (install / "usr/include/sys/mman.h").write_text("")
            for name in ("crt1.o", "libc.a", "libc.so"):
                (install / "usr/lib" / name).write_text("")

            commands = verify_busybox.musl_development_commands(install)

        self.assertLess(commands.index("mkdir /usr/include"), commands.index("mkdir /usr/include/sys"))
        self.assertIn(f"write {install}/usr/include/sys/mman.h /usr/include/sys/mman.h", commands)
        self.assertIn(f"write {install}/usr/lib/crt1.o /usr/lib/crt1.o", commands)
        self.assertIn(f"write {install}/usr/lib/libc.a /usr/lib/libc.a", commands)
        self.assertFalse(any("libc.so" in command for command in commands))


if __name__ == "__main__":
    unittest.main()
