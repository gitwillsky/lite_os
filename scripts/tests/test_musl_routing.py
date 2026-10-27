from __future__ import annotations

import importlib
import os
import subprocess
import struct
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
    def test_runtime_success_requires_both_cpu_topologies_and_real_swap_disk(self) -> None:
        module = reload_verify_musl("aarch64", "tcg")
        for fail_swap in (False, True):
            with self.subTest(fail_swap=fail_swap), tempfile.TemporaryDirectory() as directory:
                work = Path(directory)
                calls = []

                def boot(image, cpu_count, markers, **options):
                    calls.append(cpu_count)
                    self.assertIn("LiteOS musl consumer exit=0", markers)
                    self.assertIn("LiteOS musl consumer failed", options["forbidden_markers"])
                    if "scratch_disk" not in options:
                        self.assertIn("LiteOS musl pthread signal ok", markers)
                        self.assertIn("LiteOS musl ASID ok", markers)
                        return
                    self.assertIn("LiteOS musl swap ok", markers)
                    self.assertEqual(options["kernel_arguments"], "init=/bin/init -- swap-smoke")
                    self.assertEqual(options["memory"], "128M")
                    disk = options["scratch_disk"]
                    self.assertEqual(disk.stat().st_size, 64 * 1024 * 1024)
                    with disk.open("rb") as source:
                        header = source.read(4096)
                    self.assertEqual(header[4086:], b"SWAPSPACE2")
                    self.assertEqual(struct.unpack_from("<III", header, 1024), (1, 16383, 0))
                    if fail_swap and cpu_count == 2:
                        raise RuntimeError("swap regression")

                with (
                    patch.dict(os.environ, {"ARCH": "aarch64", "ACCEL": "tcg"}),
                    patch.object(module, "WORK", work),
                    patch.object(module.sys, "argv", ["verify_musl.py"]),
                    patch.object(module, "find_compiler", return_value=work / "clang"),
                    patch.object(module, "obtain_source", return_value=work / "source"),
                    patch.object(module, "obtain_linux_source", return_value=work / "linux"),
                    patch.object(module, "build_musl", return_value=(work / "sysroot", "fixture")),
                    patch.object(module, "link_smoke", return_value=work / "smoke"),
                    patch.object(module, "verify_elf"),
                    patch.object(module, "runtime_gate_payload", return_value={}),
                    patch.object(module, "runtime_gate_hit", return_value=False),
                    patch.object(module, "create_image", return_value=work / "root.img"),
                    patch.object(module, "boot", side_effect=boot),
                    patch.object(module, "publish_runtime_gate") as publish,
                ):
                    self.assertEqual(module.main(), int(fail_swap))
                    self.assertEqual(calls, [1, 2, 1, 2])
                    self.assertEqual(publish.call_count, 0 if fail_swap else 1)

    def test_dynamic_probe_requires_target_interpreter_and_only_bundled_libc(self) -> None:
        module = reload_verify_musl("aarch64", "tcg")
        static = "ELF64 AArch64 EXEC\n LOAD 0x000000 0x10000 0x10000 0x2000 0x2000 R E 0x1000\n GNU_STACK 0x0 0x0 0x0 0x0 0x0 RW 0x10"
        valid = "ELF64 AArch64 EXEC\n [Requesting program interpreter: /lib/ld-musl-aarch64.so.1]\n 0x1 (NEEDED) Shared library: [libc.so]"
        for dynamic, accepted in ((valid, True), (valid.replace("aarch64.so.1", "riscv64.so.1"), False), (valid.replace("[libc.so]", "[/host/libc.so]"), False), (valid + "\n 0x1 (NEEDED) Shared library: [extra.so]", False)):
            with self.subTest(accepted=accepted), patch.object(Path, "is_file", return_value=True), patch.object(module, "run", side_effect=(static, dynamic)):
                if accepted:
                    module.verify_elf(Path("/fixture/musl-smoke"), Path("/tool/target-gcc"))
                else:
                    with self.assertRaises(RuntimeError):
                        module.verify_elf(Path("/fixture/musl-smoke"), Path("/tool/target-gcc"))

    def test_smoke_bundle_cache_requires_dynamic_probe_and_matching_libc(self) -> None:
        module = reload_verify_musl("aarch64", "tcg")
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            with patch.object(module, "WORK", work), patch.object(module, "smoke_payload", return_value={"kind": "test"}), patch.object(module, "manifest_matches", return_value=True) as matches:
                binary = module.link_smoke(work / "sysroot", work / "clang", "sysroot-id")
                self.assertEqual(matches.call_args.args[2], ("musl-smoke", "fs-root-dynamic", "libc.so"))
                self.assertEqual(binary.name, "musl-smoke")

    def test_musl_download_falls_back_to_verified_release_source(self) -> None:
        module = reload_verify_musl("aarch64", "hvf")
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)

            def download(url: str, destination: Path) -> None:
                if url == module.MUSL_URLS[0]:
                    raise OSError("primary source unavailable")
                destination.write_bytes(b"verified musl archive")

            def digest(path: Path) -> str:
                self.assertEqual(path.read_bytes(), b"verified musl archive")
                return module.MUSL_SHA256

            with (
                patch.object(module, "WORK", work / "target"),
                patch.object(module, "SOURCE", work / ".local"),
                patch.object(module.urllib.request, "urlretrieve", side_effect=download) as retrieve,
                patch.object(module, "sha256", side_effect=digest),
                patch.object(module, "manifest_matches", return_value=True),
            ):
                source = module.obtain_source()
                expected_source = module.source_cache_path()

        self.assertEqual(
            [call.args[0] for call in retrieve.call_args_list],
            list(module.MUSL_URLS),
        )
        self.assertEqual(source, expected_source)

    def test_verify_musl_aarch64_recipe_and_cache_scope(self) -> None:
        module = reload_verify_musl("aarch64", "hvf")

        self.assertEqual(
            module.MUSL_URLS,
            (
                "https://mirrors.aliyun.com/gentoo/distfiles/9d/musl-1.2.6.tar.gz",
                "https://mirrors.tuna.tsinghua.edu.cn/gentoo/distfiles/9d/musl-1.2.6.tar.gz",
            ),
        )
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
        self.assertIn("-Wl,--image-base=0x10000", module.SMOKE_LINK_ARGUMENTS)
        self.assertNotIn("-Wl,-Ttext-segment=0x10000", module.SMOKE_LINK_ARGUMENTS)
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
                    "pinned Rust aarch64-unknown-none compiler_builtins runtime",
                ):
                    module.obtain_compiler_runtime(Path("/usr/bin/clang"))

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

    def test_riscv64_compiler_runtime_uses_its_pinned_rust_builtins(self) -> None:
        module = reload_verify_musl("riscv64", "tcg")
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory) / "lib/rustlib/riscv64gc-unknown-none-elf/lib/libcompiler_builtins-test.rlib"
            runtime.parent.mkdir(parents=True)
            runtime.touch()
            with patch.object(module, "run", return_value=directory) as run:
                selected = module.pinned_compiler_builtins()

        self.assertEqual(selected, runtime.resolve())
        self.assertEqual(
            run.call_args.args[0],
            ["rustc", "--print", "sysroot"],
        )

    def test_pinned_aarch64_compiler_runtime_links_elf_machine_183(self) -> None:
        module = reload_verify_musl("aarch64", "hvf")
        compiler = module.find_compiler()
        compiler_runtime = module.obtain_compiler_runtime(compiler)
        _, linker, archiver, _ = module.find_runtime_toolchain()
        self.assertEqual(compiler_runtime.name, "libcompiler-runtime.a")
        symbols = module.run(
            [str(archiver.with_name("llvm-nm")), "--defined-only", "--format=just-symbols", str(compiler_runtime)],
            module.ROOT,
        ).split()
        self.assertIn(module.COMPILER_RT_SUPPLEMENT_SYMBOL, symbols)
        self.assertIn("__muldc3", symbols)

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
                    # 补充对象依赖 rlib 内的 f128 arithmetic；合并 archive 必须单独闭合。
                    "-Wl,-u,__multc3",
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

    def test_musl_clang_rust_std_uses_build_std_compiler_builtins(self) -> None:
        completed = Mock(returncode=0)
        required = [Path("/tool/clang"), Path("/tool/ld.lld")]
        environment = {
            "ARCH": "aarch64",
            "LITEOS_MUSL_SYSROOT": "/sysroot",
            "LITEOS_RUST_PROVIDES_COMPILER_BUILTINS": "1",
        }
        with (
            patch.dict(os.environ, environment, clear=True),
            patch.object(sys, "argv", ["musl_clang.py", "std.o", "-o", "std-app"]),
            patch.object(musl_clang, "required_path", side_effect=required),
            patch.object(musl_clang.subprocess, "run", return_value=completed) as run,
        ):
            self.assertEqual(musl_clang.main(), 0)

        command = run.call_args.args[0]
        self.assertNotIn("/tool/compiler-builtins.rlib", command)
        self.assertEqual(required, [Path("/tool/clang"), Path("/tool/ld.lld")])

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
