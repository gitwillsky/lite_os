from __future__ import annotations

import importlib
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import apk_apps_cache  # noqa: E402
import apk_cache  # noqa: E402
import apk_rootfs  # noqa: E402
import verify_apk_apps  # noqa: E402


def reload_apk_modules(arch: str, accel: str) -> tuple[object, object, object, object]:
    with patch.dict(os.environ, {"ARCH": arch, "ACCEL": accel}, clear=True):
        cache = importlib.reload(apk_cache)
        apps_cache = importlib.reload(apk_apps_cache)
        rootfs = importlib.reload(apk_rootfs)
        verifier = importlib.reload(verify_apk_apps)
    return cache, apps_cache, rootfs, verifier


class ApkRoutingTests(unittest.TestCase):
    def test_aarch64_bootstrap_digests_are_complete(self) -> None:
        cache, _, _, _ = reload_apk_modules("aarch64", "hvf")
        packages = cache.fixed_bootstrap_packages()

        self.assertEqual(cache.WORK.name, "aarch64")
        self.assertEqual(cache.ALPINE_ARCH, "aarch64")
        self.assertEqual(cache.ALPINE_MIRROR, "https://dl-cdn.alpinelinux.org/alpine")
        self.assertTrue(cache.ALPINE_REPOSITORY.endswith("/main/aarch64"))
        self.assertEqual(set(packages), set(cache.BOOTSTRAP_PACKAGE_NAMES))
        self.assertEqual(len(packages), 3)
        self.assertEqual(
            cache.fingerprint({"packages": packages}),
            "0bd762771bab9458800691167b7bdc44491815569492f812eee6257a3833b491",
        )

    def test_riscv64_package_digests_remain_fixed(self) -> None:
        cache, apps_cache, _, _ = reload_apk_modules("riscv64", "tcg")
        payload = cache.bootstrap_payload()

        self.assertEqual(cache.WORK.name, "riscv64")
        self.assertEqual(payload["arch"], "riscv64")
        self.assertEqual(
            cache.fingerprint({"packages": payload["packages"]}),
            "beaa6b029968e99fdfd6639e07d7ac6f528bfc5b8d89ba3b0c598dd1a4fdcbe5",
        )
        self.assertEqual(
            apps_cache.fingerprint(
                {"packages": dict(apps_cache.fixed_application_packages())}
            ),
            "8422e7365c4818ce7305a67623fee77ff44825567146d93b29fc669f0b7e4420",
        )

    def test_aarch64_application_digests_are_complete(self) -> None:
        _, apps_cache, _, _ = reload_apk_modules("aarch64", "hvf")
        packages = apps_cache.fixed_application_packages()

        self.assertEqual(len(packages), 20)
        self.assertEqual(
            {name for name, _ in packages},
            {name for name, _ in apps_cache._RISCV64_APPLICATION_PACKAGES},
        )
        self.assertEqual(
            apps_cache.fingerprint({"packages": dict(packages)}),
            "42674189acfa673c9661c3529764a7978a3843542eb0d0c0b60682bdd5ee6af7",
        )

    def test_only_fixed_data_packages_accept_noarch_metadata(self) -> None:
        cache, _, _, _ = reload_apk_modules("aarch64", "hvf")
        noarch_packages = {
            "ca-certificates-bundle-20260611-r0.apk": (
                "ca-certificates-bundle",
                "20260611-r0",
            ),
            "git-init-template-2.49.1-r0.apk": ("git-init-template", "2.49.1-r0"),
            "ncurses-terminfo-base-6.5_p20250503-r0.apk": (
                "ncurses-terminfo-base",
                "6.5_p20250503-r0",
            ),
        }
        self.assertEqual(cache.FIXED_NOARCH_PACKAGES, frozenset(noarch_packages))

        for filename, (package, version) in noarch_packages.items():
            with self.subTest(filename=filename):
                with patch.object(
                    cache,
                    "run",
                    return_value=(
                        f"pkgname = {package}\npkgver = {version}\narch = noarch\n"
                    ),
                ):
                    cache.verify_package_metadata(Path(filename), filename)
                for invalid_arch in ("aarch64", "riscv64"):
                    with patch.object(
                        cache,
                        "run",
                        return_value=(
                            f"pkgname = {package}\npkgver = {version}\n"
                            f"arch = {invalid_arch}\n"
                        ),
                    ):
                        with self.assertRaisesRegex(RuntimeError, "arch=noarch"):
                            cache.verify_package_metadata(Path(filename), filename)

    def test_unlisted_noarch_package_is_rejected(self) -> None:
        cache, _, _, _ = reload_apk_modules("aarch64", "hvf")
        filename = "curl-8.14.1-r2.apk"
        with patch.object(
            cache,
            "run",
            return_value="pkgname = curl\npkgver = 8.14.1-r2\narch = noarch\n",
        ):
            with self.assertRaisesRegex(RuntimeError, "arch=aarch64"):
                cache.verify_package_metadata(Path(filename), filename)

    def test_aarch64_local_package_metadata_uses_target_dependency(self) -> None:
        _, _, rootfs, _ = reload_apk_modules("aarch64", "hvf")
        package = Path("/workspace/liteos-base.apk")
        with patch.object(rootfs, "build_signed_apk", return_value=package) as build:
            result = rootfs._build_base_package(
                Path("/workspace/rootfs"),
                Path("/workspace"),
                Path("/keys/private"),
                Path("/keys/public"),
            )

        self.assertEqual(result, package)
        metadata = build.call_args.args[2]
        self.assertEqual(metadata.arch, "aarch64")
        self.assertIn("so:libc.musl-aarch64.so.1=1.2.6", metadata.provides)

    def test_riscv64_fixture_metadata_is_preserved(self) -> None:
        _, _, rootfs, _ = reload_apk_modules("riscv64", "tcg")
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            package = workspace / "fixture.apk"
            with patch.object(rootfs, "build_signed_apk", return_value=package) as build:
                rootfs._build_fixture_package(
                    workspace,
                    Path("/keys/private"),
                    Path("/keys/public"),
                    "fixture",
                    "1.0-r0",
                    "usr/share/fixture",
                    "fixture\n",
                )

        self.assertEqual(build.call_args.args[2].arch, "riscv64")

    def test_apk_app_artifacts_and_work_are_target_scoped(self) -> None:
        _, _, _, verifier = reload_apk_modules("aarch64", "hvf")

        self.assertEqual(verifier.WORK.name, "aarch64")
        self.assertEqual(
            verifier.target_runtime_artifacts(),
            (
                verifier.ROOT
                / "target/aarch64-unknown-none-softfloat/release/kernel",
            ),
        )


if __name__ == "__main__":
    unittest.main()
