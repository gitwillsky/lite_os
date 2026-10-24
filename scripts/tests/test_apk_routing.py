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

        self.assertEqual(cache.SOURCE.name, "aarch64")
        self.assertEqual(cache.ALPINE_ARCH, "aarch64")
        self.assertEqual(cache.ALPINE_MIRROR, "https://mirrors.ustc.edu.cn/alpine")
        self.assertEqual(cache.ALPINE_PACKAGE_REPOSITORIES, (
            "https://mirrors.ustc.edu.cn/alpine/v3.22/main/aarch64",
            "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/aarch64",
        ))
        self.assertEqual(set(packages), set(cache.BOOTSTRAP_PACKAGE_NAMES))
        self.assertEqual(len(packages), 3)
        self.assertEqual(
            cache.fingerprint({"packages": packages}),
            "d7685b6b43bed1900c0e3472e73bd604ee00b67d07612fcb5bd46897fed7feab",
        )

    def test_riscv64_package_digests_remain_fixed(self) -> None:
        cache, apps_cache, _, _ = reload_apk_modules("riscv64", "tcg")
        payload = cache.bootstrap_payload()

        self.assertEqual(cache.SOURCE.name, "riscv64")
        self.assertEqual(payload["arch"], "riscv64")
        self.assertEqual(
            cache.fingerprint({"packages": payload["packages"]}),
            "2f667920b31a3f252014c4ef75d9c2c89eb4276e4a0cf474ef0841ccaa1598ff",
        )
        self.assertEqual(
            apps_cache.fingerprint(
                {"packages": dict(apps_cache.fixed_application_packages())}
            ),
            "19be8d8ed52f4152d85bf945fbc56fd73a3749c9cf7a66ae75075c400ea98ee7",
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
            "79e6b37eabb8e23f7f8a9b7ad18fc5c3912eb80b81ea10b0b0a6993dfb6ffe4b",
        )

    def test_only_fixed_data_packages_accept_noarch_metadata(self) -> None:
        cache, _, _, _ = reload_apk_modules("aarch64", "hvf")
        noarch_packages = {
            "ca-certificates-bundle-20261020-r0.apk": (
                "ca-certificates-bundle",
                "20261020-r0",
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

    def test_application_install_queries_match_both_architecture_pins(self) -> None:
        import re
        fixture = (SCRIPTS / "fixtures/apk-apps/install.sh").read_text()
        queries = re.findall(r"\$APK info -e '([^']+)'", fixture)
        self.assertEqual({query.split("=", 1)[0] for query in queries}, {"curl", "sqlite", "git"})
        for arch in ("aarch64", "riscv64"):
            _, apps_cache, _, _ = reload_apk_modules(arch, "tcg")
            packages = dict(apps_cache.fixed_application_packages())
            for query in queries:
                name, version = query.split("=", 1)
                self.assertIn(f"{name}-{version}.apk", packages, f"{arch}: stale install query {query}")

    def test_unlisted_noarch_package_is_rejected(self) -> None:
        cache, _, _, _ = reload_apk_modules("aarch64", "hvf")
        filename = "curl-8.14.1-r3.apk"
        with patch.object(
            cache,
            "run",
            return_value="pkgname = curl\npkgver = 8.14.1-r3\narch = noarch\n",
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

    def test_runtime_repositories_enable_only_stable_main_and_community(self) -> None:
        _, _, rootfs, _ = reload_apk_modules("aarch64", "hvf")

        self.assertEqual(
            rootfs.ALPINE_RUNTIME_REPOSITORIES,
            (
                "https://mirrors.ustc.edu.cn/alpine/v3.22/main",
                "https://mirrors.ustc.edu.cn/alpine/v3.22/community",
            ),
        )

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
