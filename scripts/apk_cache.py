#!/usr/bin/env python3
"""获取固定 Alpine APK bootstrap，并管理本地 LiteOS repository 签名身份。"""

from __future__ import annotations

import os
import shutil
import subprocess
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from build_target import target_from_environment
from build_cache import (
    cache_lock,
    fingerprint,
    manifest_matches,
    publish_directory,
    sha256,
    temporary_directory,
    write_manifest,
)

ROOT = Path(__file__).resolve().parent.parent
TARGET = target_from_environment()
WORK = ROOT / "target" / "apk-runtime" / TARGET.arch
ALPINE_BRANCH = "v3.22"
ALPINE_ARCH = TARGET.alpine_arch
ALPINE_MIRROR = "https://dl-cdn.alpinelinux.org/alpine"
ALPINE_REPOSITORY = (
    f"{ALPINE_MIRROR}/{ALPINE_BRANCH}/main/{ALPINE_ARCH}"
)
BOOTSTRAP_PACKAGE_NAMES = (
    "apk-tools-static-2.14.10-r0.apk",
    "alpine-keys-2.5-r0.apk",
    "ca-certificates-bundle-20260611-r0.apk",
)
_BOOTSTRAP_PACKAGES_BY_ARCH: dict[str, dict[str, str]] = {
    "riscv64": {
        BOOTSTRAP_PACKAGE_NAMES[0]: (
            "85419c4d80eceb12af9cc3be178dce3599ef04679c46eee25175b6673c14cd43"
        ),
        BOOTSTRAP_PACKAGE_NAMES[1]: (
            "ca4835c8907791ab172fc64e53a81ab4ed06ff21c493d2a7fe8f66a80e2ea200"
        ),
        BOOTSTRAP_PACKAGE_NAMES[2]: (
            "537dcb625ede1cb81e751dd92552b2715a35fdd72cdb43a965a055f14900d529"
        ),
    },
    "aarch64": {
        BOOTSTRAP_PACKAGE_NAMES[0]: (
            "3e22f80dd0272dc487e4ca84b2c6b660ca392cbad970764efe9ef9555b806ac8"
        ),
        BOOTSTRAP_PACKAGE_NAMES[1]: (
            "2e4c85ae16cabeb53b4145006f883bf8e57d454bd3faff14d35ec7d8a0d05b1a"
        ),
        BOOTSTRAP_PACKAGE_NAMES[2]: (
            "ae45c92eba28db3434058980c40930d3653663e5251cb04c9fd49a94ca00c93b"
        ),
    },
}
FIXED_NOARCH_PACKAGES = frozenset(
    {
        "ca-certificates-bundle-20260611-r0.apk",
        "git-init-template-2.49.1-r0.apk",
        "ncurses-terminfo-base-6.5_p20250503-r0.apk",
    }
)
BOOTSTRAP_RECIPE_VERSION = 3
LOCAL_KEY_NAME = "liteos-local.rsa"


@dataclass(frozen=True)
class ApkBootstrapPaths:
    """@description 已校验并原子发布的 APK bootstrap 输入。"""

    apk_static: Path
    alpine_keys: Path
    ca_certificates_bundle: Path
    private_key: Path
    public_key: Path
    fingerprint: str


def fixed_bootstrap_packages() -> dict[str, str]:
    """返回当前架构的固定 bootstrap 摘要，缺失时在下载前 fail-stop。"""
    packages = _BOOTSTRAP_PACKAGES_BY_ARCH[ALPINE_ARCH]
    missing = [name for name in BOOTSTRAP_PACKAGE_NAMES if name not in packages]
    if missing:
        raise RuntimeError(
            f"fixed Alpine {ALPINE_BRANCH} SHA-256 values are missing for "
            f"{ALPINE_ARCH}: {', '.join(missing)}"
        )
    return packages


def run(command: list[str], cwd: Path = ROOT) -> str:
    """执行 bootstrap 构建命令，并在失败时保留可诊断输出。"""
    result = subprocess.run(
        command,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-80:])
        raise RuntimeError(f"command failed: {' '.join(command)}\n{tail}")
    return result.stdout


def verify_package_metadata(archive: Path, name: str) -> None:
    """验证固定 APK 的包名、版本与精确架构声明。"""
    metadata: dict[str, str] = {}
    for line in run(["tar", "-xOf", str(archive), ".PKGINFO"]).splitlines():
        key, separator, value = line.partition(" = ")
        if separator and key in {"pkgname", "pkgver", "arch"}:
            if key in metadata:
                raise RuntimeError(f"duplicate Alpine package metadata {key}: {name}")
            metadata[key] = value

    actual_name = metadata.get("pkgname", "<missing>")
    actual_version = metadata.get("pkgver", "<missing>")
    actual_arch = metadata.get("arch", "<missing>")
    expected_arch = "noarch" if name in FIXED_NOARCH_PACKAGES else ALPINE_ARCH
    expected_name = f"{actual_name}-{actual_version}.apk"
    if name != expected_name or actual_arch != expected_arch:
        raise RuntimeError(
            f"Alpine package metadata mismatch: {name}; expected filename={name}, "
            f"arch={expected_arch}; actual filename={expected_name}, arch={actual_arch}"
        )


def download(name: str, expected_sha256: str) -> Path:
    """下载固定 APK；校验失败的文件绝不进入后续 extraction。"""
    archives = WORK / "archives"
    archives.mkdir(parents=True, exist_ok=True)
    archive = archives / name
    if archive.is_file() and sha256(archive) == expected_sha256:
        verify_package_metadata(archive, name)
        return archive
    archive.unlink(missing_ok=True)
    temporary = archive.with_suffix(".download")
    temporary.unlink(missing_ok=True)
    try:
        urllib.request.urlretrieve(f"{ALPINE_REPOSITORY}/{name}", temporary)
    except Exception as error:
        temporary.unlink(missing_ok=True)
        raise RuntimeError(f"failed to download fixed Alpine package {name}: {error}") from error
    if sha256(temporary) != expected_sha256:
        temporary.unlink(missing_ok=True)
        raise RuntimeError(f"Alpine package SHA-256 mismatch: {name}")
    try:
        verify_package_metadata(temporary, name)
    except Exception:
        temporary.unlink(missing_ok=True)
        raise
    os.replace(temporary, archive)
    return archive


def bootstrap_payload() -> dict[str, object]:
    """返回不受镜像输出路径影响的 bootstrap cache identity。"""
    packages = fixed_bootstrap_packages()
    return {
        "kind": "apk-bootstrap",
        "recipe_version": BOOTSTRAP_RECIPE_VERSION,
        "branch": ALPINE_BRANCH,
        "arch": ALPINE_ARCH,
        "packages": packages,
    }


def obtain_bootstrap() -> tuple[Path, Path, str]:
    """提取 dependency-free apk.static 与 Alpine 官方 repository keys。"""
    packages = fixed_bootstrap_packages()
    payload = bootstrap_payload()
    identity = fingerprint(payload)
    extracted = WORK / "bootstraps" / identity
    required = ("sbin/apk.static", "etc/apk/keys")
    if manifest_matches(extracted, payload, (required[0],)) and (extracted / required[1]).is_dir():
        return extracted / required[0], extracted / required[1], identity

    temporary = temporary_directory(WORK / "bootstraps", "bootstrap")
    try:
        for name in BOOTSTRAP_PACKAGE_NAMES[:2]:
            run(
                ["tar", "-xf", str(download(name, packages[name])), "-C", str(temporary)]
            )
        apk_static = temporary / required[0]
        keys = temporary / required[1]
        if not apk_static.is_file() or not keys.is_dir() or not any(keys.iterdir()):
            raise RuntimeError("fixed Alpine packages lack apk.static or repository keys")
        write_manifest(temporary, payload)
        publish_directory(temporary, extracted)
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    return extracted / required[0], extracted / required[1], identity


def obtain_local_signing_key() -> tuple[Path, Path]:
    """创建 repository-local key；private material 只存在 target cache，不进入仓库。"""
    keys = WORK / "local-keys"
    private_key = keys / LOCAL_KEY_NAME
    public_key = keys / f"{LOCAL_KEY_NAME}.pub"
    if private_key.is_file() and public_key.is_file():
        return private_key, public_key
    temporary = temporary_directory(WORK, "local-keys")
    try:
        temporary_private = temporary / LOCAL_KEY_NAME
        temporary_public = temporary / f"{LOCAL_KEY_NAME}.pub"
        run(["openssl", "genrsa", "-out", str(temporary_private), "2048"])
        run(
            [
                "openssl",
                "rsa",
                "-in",
                str(temporary_private),
                "-pubout",
                "-out",
                str(temporary_public),
            ]
        )
        temporary_private.chmod(0o600)
        publish_directory(temporary, keys)
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    return private_key, public_key


def cached_apk_bootstrap() -> ApkBootstrapPaths:
    """返回完整 APK bootstrap；调用者不感知 archive/extraction/key storage。"""
    packages = fixed_bootstrap_packages()
    with cache_lock(WORK / ".bootstrap.lock"):
        apk_static, alpine_keys, bootstrap_fingerprint = obtain_bootstrap()
        private_key, public_key = obtain_local_signing_key()
        identity = fingerprint(
            {
                "bootstrap": bootstrap_fingerprint,
                "local_public_key_sha256": sha256(public_key),
            }
        )
        return ApkBootstrapPaths(
            apk_static=apk_static.resolve(),
            alpine_keys=alpine_keys.resolve(),
            ca_certificates_bundle=download(
                BOOTSTRAP_PACKAGE_NAMES[2], packages[BOOTSTRAP_PACKAGE_NAMES[2]]
            ).resolve(),
            private_key=private_key.resolve(),
            public_key=public_key.resolve(),
            fingerprint=identity,
        )
