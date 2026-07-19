from __future__ import annotations

import importlib
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import musl_clang  # noqa: E402
import musl_ld  # noqa: E402
import verify_musl  # noqa: E402


def reload_verify_musl(arch: str, accel: str) -> object:
    with patch.dict(os.environ, {"ARCH": arch, "ACCEL": accel}, clear=True):
        return importlib.reload(verify_musl)


class MuslRoutingTests(unittest.TestCase):
    def test_verify_musl_aarch64_recipe_and_cache_scope(self) -> None:
        module = reload_verify_musl("aarch64", "hvf")

        self.assertEqual(module.CONFIGURE_ARGUMENTS[0], "--target=aarch64")
        self.assertEqual(module.LINUX_HEADER_ARCH, "arm64")
        self.assertEqual(module.ELF_MACHINE, "AArch64")
        self.assertIn("-march=armv8-a", module.SMOKE_LINK_ARGUMENTS)
        self.assertIn("-Wl,--image-base=0x10000", module.SMOKE_LINK_ARGUMENTS)
        self.assertNotIn("-Wl,-Ttext-segment=0x10000", module.SMOKE_LINK_ARGUMENTS)
        self.assertNotIn("-march=rv64gc", module.SMOKE_LINK_ARGUMENTS)
        self.assertEqual(module.WORK.name, "aarch64")
        self.assertEqual(module.TARGET.musl_loader, "/lib/ld-musl-aarch64.so.1")

    def test_verify_musl_riscv64_recipe_remains_scoped(self) -> None:
        module = reload_verify_musl("riscv64", "tcg")

        self.assertEqual(module.CONFIGURE_ARGUMENTS[0], "--target=riscv64")
        self.assertEqual(module.LINUX_HEADER_ARCH, "riscv")
        self.assertEqual(module.ELF_MACHINE, "RISC-V")
        self.assertIn("-march=rv64gc", module.SMOKE_LINK_ARGUMENTS)
        self.assertIn("-mabi=lp64d", module.SMOKE_LINK_ARGUMENTS)
        self.assertIn("-Wl,-Ttext-segment=0x10000", module.SMOKE_LINK_ARGUMENTS)
        self.assertNotIn("-Wl,--image-base=0x10000", module.SMOKE_LINK_ARGUMENTS)
        self.assertEqual(module.WORK.name, "riscv64")
        self.assertEqual(module.TARGET.musl_loader, "/lib/ld-musl-riscv64.so.1")

    def test_runtime_toolchain_probe_uses_aarch64_backend(self) -> None:
        module = reload_verify_musl("aarch64", "hvf")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            clang = root / "clang"
            archiver = root / "llvm-ar"
            ranlib = root / "llvm-ranlib"
            linker = root / "lib/rustlib/aarch64/bin/rust-lld"
            for path in (clang, archiver, ranlib, linker):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.touch()

            def which(name: str) -> str | None:
                return {
                    "clang": str(clang),
                    "llvm-ar": str(archiver),
                    "llvm-ranlib": str(ranlib),
                }.get(name)

            targets = subprocess.CompletedProcess(
                args=[],
                returncode=0,
                stdout="  aarch64 - AArch64 (little endian)\n",
            )
            with (
                patch.object(module.shutil, "which", side_effect=which),
                patch.object(module.subprocess, "run", return_value=targets),
                patch.object(module, "run", return_value=str(root)),
            ):
                selected = module.find_runtime_toolchain()

        self.assertEqual(selected[0], clang.resolve())

    def test_missing_aarch64_compiler_runtime_is_explicit(self) -> None:
        module = reload_verify_musl("aarch64", "hvf")
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(module, "run", return_value=directory) as run:
                with self.assertRaisesRegex(
                    RuntimeError,
                    "aarch64-unknown-none hard-float compiler_builtins runtime",
                ):
                    module.find_compiler_runtime(Path("/usr/bin/clang"))

        self.assertNotIn("MAKEFLAGS", run.call_args.kwargs["env"])
        self.assertNotIn("MFLAGS", run.call_args.kwargs["env"])

    def test_rustc_probe_drops_stale_make_jobserver_capability(self) -> None:
        module = reload_verify_musl("aarch64", "hvf")
        with patch.dict(
            os.environ,
            {
                "KEEP_ME": "yes",
                "MAKEFLAGS": "--jobserver-fds=3,4 -j",
                "MFLAGS": "-j",
            },
            clear=True,
        ):
            environment = module.rustc_probe_environment()

        self.assertEqual(environment, {"KEEP_ME": "yes"})

    def test_riscv64_compiler_runtime_remains_gcc_libgcc(self) -> None:
        module = reload_verify_musl("riscv64", "tcg")
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory) / "libgcc.a"
            runtime.touch()
            compiler = Path("/tool/riscv64-unknown-elf-gcc")
            with patch.object(module, "run", return_value=str(runtime)) as run:
                selected = module.find_compiler_runtime(compiler)

        self.assertEqual(selected, runtime.resolve())
        self.assertEqual(
            run.call_args.args[0],
            [str(compiler), "-print-libgcc-file-name"],
        )

    def test_pinned_aarch64_compiler_runtime_links_elf_machine_183(self) -> None:
        module = reload_verify_musl("aarch64", "hvf")
        compiler = module.find_compiler()
        compiler_runtime = module.find_compiler_runtime(compiler)
        _, linker, _, _ = module.find_runtime_toolchain()
        self.assertEqual(compiler.name, "clang")
        self.assertEqual(compiler_runtime.suffix, ".rlib")

        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            empty = workspace / "empty.o"
            output = workspace / "minimal"
            linker_alias = workspace / "ld.lld"
            linker_alias.symlink_to(linker)
            subprocess.run(
                [
                    str(compiler),
                    "--target=aarch64-linux-musl",
                    "-c",
                    "-x",
                    "c",
                    "/dev/null",
                    "-o",
                    str(empty),
                ],
                check=True,
            )
            subprocess.run(
                [
                    str(compiler),
                    "--target=aarch64-linux-musl",
                    f"--ld-path={linker_alias}",
                    "-nostdlib",
                    "-Wl,-e,0",
                    "-Wl,-u,__divti3",
                    str(empty),
                    str(compiler_runtime),
                    "-o",
                    str(output),
                ],
                check=True,
            )
            elf_header = output.read_bytes()[:20]

        self.assertEqual(elf_header[:4], b"\x7fELF")
        self.assertEqual(int.from_bytes(elf_header[18:20], "little"), 183)

    def test_musl_clang_aarch64_dynamic_link_route(self) -> None:
        completed = Mock(returncode=0)
        required = [
            Path("/tool/clang"),
            Path("/tool/ld.lld"),
            Path("/tool/compiler-builtins.rlib"),
        ]
        environment = {"ARCH": "aarch64", "LITEOS_MUSL_SYSROOT": "/sysroot"}
        with (
            patch.dict(os.environ, environment, clear=True),
            patch.object(sys, "argv", ["musl_clang.py", "main.o", "-o", "app"]),
            patch.object(musl_clang, "required_path", side_effect=required),
            patch.object(musl_clang.subprocess, "run", return_value=completed) as run,
        ):
            self.assertEqual(musl_clang.main(), 0)

        command = run.call_args.args[0]
        self.assertIn("--target=aarch64-linux-musl", command)
        self.assertIn("-Wl,-dynamic-linker,/lib/ld-musl-aarch64.so.1", command)

    def test_musl_clang_riscv64_route_is_preserved(self) -> None:
        completed = Mock(returncode=0)
        required = [Path("/tool/clang"), Path("/tool/ld.lld"), Path("/tool/libgcc.a")]
        environment = {
            "ARCH": "riscv64",
            "ACCEL": "tcg",
            "LITEOS_MUSL_SYSROOT": "/sysroot",
        }
        with (
            patch.dict(os.environ, environment, clear=True),
            patch.object(sys, "argv", ["musl_clang.py", "main.o", "-o", "app"]),
            patch.object(musl_clang, "required_path", side_effect=required),
            patch.object(musl_clang.subprocess, "run", return_value=completed) as run,
        ):
            self.assertEqual(musl_clang.main(), 0)

        command = run.call_args.args[0]
        self.assertIn("--target=riscv64-linux-musl", command)
        self.assertIn("-Wl,-dynamic-linker,/lib/ld-musl-riscv64.so.1", command)

    def test_musl_ld_empty_relocatable_uses_selected_target(self) -> None:
        completed = Mock(returncode=0)
        environment = {
            "ARCH": "aarch64",
            "LITEOS_MUSL_LLD": "/tool/ld.lld",
            "LITEOS_MUSL_CLANG": "/tool/clang",
        }
        with (
            patch.dict(os.environ, environment, clear=True),
            patch.object(sys, "argv", ["musl_ld.py", "-r", "-o", "empty.o"]),
            patch.object(musl_ld.subprocess, "run", return_value=completed) as run,
        ):
            self.assertEqual(musl_ld.main(), 0)

        self.assertIn("--target=aarch64-linux-musl", run.call_args.args[0])


if __name__ == "__main__":
    unittest.main()
