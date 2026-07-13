#!/usr/bin/env python3
"""获取固定 Alpine APK bootstrap，并管理本地 LiteOS repository 签名身份。"""

from __future__ import annotations

import os
import shutil
import subprocess
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from build_cache import (
    fingerprint,
    manifest_matches,
    publish_directory,
    sha256,
    temporary_directory,
    write_manifest,
)

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target" / "apk-runtime"
ALPINE_BRANCH = "v3.22"
ALPINE_ARCH = "riscv64"
ALPINE_MIRROR = "https://mirrors.ustc.edu.cn/alpine"
ALPINE_REPOSITORY = (
    f"{ALPINE_MIRROR}/{ALPINE_BRANCH}/main/{ALPINE_ARCH}"
)
APK_TOOLS_STATIC = (
    "apk-tools-static-2.14.10-r0.apk",
    "85419c4d80eceb12af9cc3be178dce3599ef04679c46eee25175b6673c14cd43",
)
ALPINE_KEYS = (
    "alpine-keys-2.5-r0.apk",
    "ca4835c8907791ab172fc64e53a81ab4ed06ff21c493d2a7fe8f66a80e2ea200",
)
CA_CERTIFICATES_BUNDLE = (
    "ca-certificates-bundle-20260611-r0.apk",
    "537dcb625ede1cb81e751dd92552b2715a35fdd72cdb43a965a055f14900d529",
)
BOOTSTRAP_RECIPE_VERSION = 2
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


def download(name: str, expected_sha256: str) -> Path:
    """下载固定 APK；校验失败的文件绝不进入后续 extraction。"""
    archives = WORK / "archives"
    archives.mkdir(parents=True, exist_ok=True)
    archive = archives / name
    if archive.is_file() and sha256(archive) == expected_sha256:
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
    os.replace(temporary, archive)
    return archive


def bootstrap_payload() -> dict[str, object]:
    """返回不受镜像输出路径影响的 bootstrap cache identity。"""
    return {
        "kind": "apk-bootstrap",
        "recipe_version": BOOTSTRAP_RECIPE_VERSION,
        "branch": ALPINE_BRANCH,
        "arch": ALPINE_ARCH,
        "packages": {
            APK_TOOLS_STATIC[0]: APK_TOOLS_STATIC[1],
            ALPINE_KEYS[0]: ALPINE_KEYS[1],
            CA_CERTIFICATES_BUNDLE[0]: CA_CERTIFICATES_BUNDLE[1],
        },
    }


def obtain_bootstrap() -> tuple[Path, Path, str]:
    """提取 dependency-free apk.static 与 Alpine 官方 repository keys。"""
    payload = bootstrap_payload()
    identity = fingerprint(payload)
    extracted = WORK / "bootstraps" / identity
    required = ("sbin/apk.static", "etc/apk/keys")
    if manifest_matches(extracted, payload, (required[0],)) and (extracted / required[1]).is_dir():
        return extracted / required[0], extracted / required[1], identity

    temporary = temporary_directory(WORK / "bootstraps", "bootstrap")
    try:
        for name, digest in (APK_TOOLS_STATIC, ALPINE_KEYS):
            run(["tar", "-xf", str(download(name, digest)), "-C", str(temporary)])
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
        ca_certificates_bundle=download(*CA_CERTIFICATES_BUNDLE).resolve(),
        private_key=private_key.resolve(),
        public_key=public_key.resolve(),
        fingerprint=identity,
    )
