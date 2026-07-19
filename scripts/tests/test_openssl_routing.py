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

import openssl_cache  # noqa: E402


def reload_openssl(arch: str, accel: str) -> object:
    with patch.dict(os.environ, {"ARCH": arch, "ACCEL": accel}, clear=True):
        return importlib.reload(openssl_cache)


class OpenSslRoutingTests(unittest.TestCase):
    def tearDown(self) -> None:
        reload_openssl("aarch64", "hvf")

    def test_configure_target_is_exact_for_each_build_target(self) -> None:
        cases = (
            ("aarch64", "hvf", "linux-aarch64"),
            ("riscv64", "tcg", "linux64-riscv64"),
        )
        for arch, accel, expected in cases:
            with self.subTest(arch=arch):
                module = reload_openssl(arch, accel)
                self.assertEqual(module.TARGET.arch, arch)
                self.assertEqual(module.OPENSSL_CONFIGURE_TARGET, expected)

    def test_unknown_architecture_is_rejected_before_mapping(self) -> None:
        with patch.dict(
            os.environ,
            {"ARCH": "unknown", "ACCEL": "tcg"},
            clear=True,
        ):
            with self.assertRaisesRegex(ValueError, "ARCH must be one of"):
                importlib.reload(openssl_cache)

    def test_payload_argv_and_toolchain_environment_share_target_owner(self) -> None:
        module = reload_openssl("aarch64", "hvf")
        musl = Mock()
        musl.sysroot_fingerprint = "aarch64-sysroot"
        musl.compiler = Path("/tool/aarch64-clang")
        musl.linker = Path("/tool/aarch64-ld.lld")
        musl.compiler_runtime = Path("/tool/aarch64-builtins.rlib")
        musl.install = Path("/sysroot/aarch64")
        musl.archiver = Path("/tool/llvm-ar")

        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            source = work / "source"
            source.mkdir()
            configure_calls: list[tuple[list[str], dict[str, str]]] = []
            manifest_payloads: list[dict[str, object]] = []

            def run(command: list[str], cwd: Path, env=None) -> str:
                if command[0].endswith("/Configure"):
                    configure_calls.append((command, env))
                elif "build_programs" in command:
                    built = cwd / "apps/openssl"
                    built.parent.mkdir(parents=True)
                    built.touch()
                elif "llvm-strip" in command[0]:
                    Path(command[2]).touch()
                return ""

            with (
                patch.object(module, "WORK", work),
                patch.object(module, "_source", return_value=(source, "source-id")),
                patch.object(module, "_ca_bundle", return_value=work / "cacert.pem"),
                patch.object(module, "compiler_identity", return_value={"target": "aarch64"}),
                patch.object(module, "run", side_effect=run),
                patch.object(
                    module,
                    "write_manifest",
                    side_effect=lambda _, payload: manifest_payloads.append(payload),
                ),
                patch.object(module, "publish_generation"),
            ):
                module.build_openssl(musl, jobs_override=1, rebuild=True)

        self.assertEqual(len(configure_calls), 1)
        command, environment = configure_calls[0]
        self.assertEqual(command[1], module.OPENSSL_CONFIGURE_TARGET)
        self.assertEqual(
            manifest_payloads[0]["configure_target"],
            module.OPENSSL_CONFIGURE_TARGET,
        )
        self.assertEqual(environment["LITEOS_MUSL_CLANG"], str(musl.compiler))
        self.assertEqual(environment["LITEOS_MUSL_LLD"], str(musl.linker))
        self.assertEqual(
            environment["LITEOS_MUSL_COMPILER_RUNTIME"],
            str(musl.compiler_runtime),
        )
        self.assertEqual(environment["LITEOS_MUSL_SYSROOT"], str(musl.install))
        self.assertEqual(environment["AR"], str(musl.archiver))


if __name__ == "__main__":
    unittest.main()
