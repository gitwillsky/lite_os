#!/usr/bin/env python3
"""获取 LiteOS AArch64 Agent 开发环境的固定 CLI 与 APK 输入。"""

from __future__ import annotations

import gzip
import io
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from apk_apps_cache import cached_application_apks
from apk_cache import (
    ALPINE_ARCH,
    ALPINE_BRANCH,
    ALPINE_MIRROR,
    cached_apk_bootstrap,
)
from apk_package import add_apk_signature, split_apk_gzip_members
from build_cache import (
    cache_lock,
    fingerprint,
    manifest_matches,
    publish_directory,
    sha256,
    temporary_directory,
    write_manifest,
)
from build_target import target_from_environment

ROOT = Path(__file__).resolve().parent.parent
TARGET = target_from_environment()
WORK = ROOT / "target" / "agent-development" / TARGET.arch

CODEX_VERSION = "0.145.0"
CODEX_ARCHIVE_NAME = "codex-aarch64-unknown-linux-musl.tar.gz"
CODEX_ARCHIVE_URL = (
    "https://github.com/openai/codex/releases/download/"
    f"rust-v{CODEX_VERSION}/{CODEX_ARCHIVE_NAME}"
)
CODEX_ARCHIVE_SHA256 = (
    "d384f90bc842450b42bd675feef06a12a46a3b1ca97efcb22566b270e4a11227"
)
CODEX_BINARY_NAME = "codex-aarch64-unknown-linux-musl"

CLAUDE_VERSION = "2.1.212-r1"
CLAUDE_APK_NAME = f"claude-code-{CLAUDE_VERSION}.apk"
CLAUDE_APK_URL = (
    "https://downloads.claude.ai/claude-code/apk/stable/"
    f"aarch64/{CLAUDE_APK_NAME}"
)
CLAUDE_APK_SHA256 = (
    "a7b800b0a1e392c5facd7743425711d8f2da6278600f69c18299d8d90469244a"
)
CLAUDE_KEY_NAME = "claude-code.rsa.pub"
CLAUDE_KEY_URL = f"https://downloads.claude.ai/keys/{CLAUDE_KEY_NAME}"
CLAUDE_KEY_SHA256 = (
    "395759c1f7449ef4cdef305a42e820f3c766d6090d142634ebdb049f113168b6"
)
CLAUDE_INDEX_NAME = "APKINDEX.tar.gz"
CLAUDE_INDEX_GENERATION = "1784943401844049"
CLAUDE_INDEX_URL = (
    "https://downloads.claude.ai/claude-code/apk/stable/aarch64/"
    f"{CLAUDE_INDEX_NAME}?generation={CLAUDE_INDEX_GENERATION}"
)
CLAUDE_INDEX_SHA256 = (
    "131c9ed4cb32b8d0ebcfed34b75ece690d76d9b0f5814134361eccf85bfea80f"
)

# Agent 开发镜像需要 Bash、外部 ripgrep 与 Claude 的 C++ runtime。缺失这些固定包时，
# Claude 在 Alpine 上会选择不受支持的 bundled rg 或在 loader 阶段直接缺少 shared object。
AGENT_ALPINE_PACKAGES = (
    (
        "main",
        "bash-5.2.37-r0.apk",
        "411e1fec2dccd603bc9f23586f7b8df2211613ece49b20c71c17412ab2667c44",
    ),
    (
        "main",
        "libgcc-14.2.0-r6.apk",
        "ba1835eec3ad8a120efd3d5020e561d53553a0513763a08f509e3ce6d4baa9ca",
    ),
    (
        "main",
        "libstdc++-14.2.0-r6.apk",
        "0d2f054057a4f932e985a129eccb79908b40964185139a0a609aed3032aba064",
    ),
    (
        "community",
        "ripgrep-14.1.1-r0.apk",
        "f9c145aca9868a3a90d57d4eb89a4c1c92bc4f06870311d230856f68cf6e58bd",
    ),
)
RECIPE_VERSION = 1


@dataclass(frozen=True)
class AgentCliArtifacts:
    """已校验并缓存的 Agent CLI、签名身份与离线 APK 闭包。"""

    codex_binary: Path
    claude_key: Path
    claude_index: Path
    claude_apk: Path
    alpine_apks: tuple[Path, ...]
    fingerprint: str


def ensure_supported_target() -> None:
    """确认当前 target 能消费固定的 AArch64 Agent 原生产物。

    Raises:
        RuntimeError: 当前架构不是一等 AArch64 开发路径。
    """
    if TARGET.arch != "aarch64" or ALPINE_ARCH != "aarch64":
        raise RuntimeError(
            "Agent development image currently supports only ARCH=aarch64"
        )


def _run(command: list[str]) -> str:
    result = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-60:])
        raise RuntimeError(f"command failed: {' '.join(command)}\n{tail}")
    return result.stdout


def _download(url: str, name: str, expected_sha256: str) -> Path:
    """下载一个固定输入，并只在摘要匹配后发布到共享 cache。"""
    archives = WORK / "archives"
    archives.mkdir(parents=True, exist_ok=True)
    destination = archives / name
    if destination.is_file() and sha256(destination) == expected_sha256:
        return destination

    destination.unlink(missing_ok=True)
    temporary = destination.with_suffix(destination.suffix + ".download")
    temporary.unlink(missing_ok=True)
    try:
        urllib.request.urlretrieve(url, temporary)
        actual = sha256(temporary)
        if actual != expected_sha256:
            raise RuntimeError(
                f"Agent artifact SHA-256 mismatch: {name}; "
                f"expected={expected_sha256}, actual={actual}"
            )
        os.replace(temporary, destination)
    except Exception:
        temporary.unlink(missing_ok=True)
        raise
    return destination


def _verify_apk(archive: Path, package: str, version: str) -> None:
    """验证固定 APK 的 package/version/architecture metadata。"""
    metadata: dict[str, str] = {}
    for line in _run(["tar", "-xOf", str(archive), ".PKGINFO"]).splitlines():
        key, separator, value = line.partition(" = ")
        if separator and key in {"pkgname", "pkgver", "arch"}:
            metadata[key] = value
    expected = {"pkgname": package, "pkgver": version, "arch": "aarch64"}
    if metadata != expected:
        raise RuntimeError(
            f"Agent APK metadata mismatch: {archive.name}; "
            f"expected={expected}, actual={metadata}"
        )


def _verify_claude_index(index: Path, public_key: Path) -> None:
    """验证固定官方 index 的签名成员与目标 Claude package metadata。"""
    members = split_apk_gzip_members(index.read_bytes())
    if len(members) != 2:
        raise RuntimeError("Claude APK index must contain signature and index members")
    with tarfile.open(
        fileobj=io.BytesIO(gzip.decompress(members[0])),
        mode="r:",
    ) as archive:
        names = set(archive.getnames())
        expected_signature = f".SIGN.RSA512.{CLAUDE_KEY_NAME}"
        if names != {expected_signature}:
            raise RuntimeError(f"Claude APK index lacks {expected_signature}")
        source = archive.extractfile(expected_signature)
        if source is None:
            raise RuntimeError("Claude APK index signature is unreadable")
        signature = source.read()
    with tempfile.TemporaryDirectory(prefix="liteos-claude-index-verify-") as temporary:
        directory = Path(temporary)
        signature_path = directory / "signature"
        payload_path = directory / "APKINDEX.tar.gz"
        signature_path.write_bytes(signature)
        payload_path.write_bytes(members[1])
        result = subprocess.run(
            [
                "openssl",
                "dgst",
                "-sha512",
                "-verify",
                str(public_key),
                "-signature",
                str(signature_path),
                str(payload_path),
            ],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        if result.returncode != 0 or "Verified OK" not in result.stdout:
            raise RuntimeError(f"Claude APK index signature is invalid: {result.stdout}")

    with tarfile.open(
        fileobj=io.BytesIO(gzip.decompress(members[1])),
        mode="r:",
    ) as archive:
        source = archive.extractfile("APKINDEX")
        if source is None:
            raise RuntimeError("Claude APK index payload is unreadable")
        entries = source.read().decode().strip().split("\n\n")
    expected = {
        "P": "claude-code",
        "V": CLAUDE_VERSION,
        "A": "aarch64",
    }
    for entry in entries:
        metadata = {
            key: value
            for line in entry.splitlines()
            for key, separator, value in (line.partition(":"),)
            if separator and key in expected
        }
        if metadata == expected:
            return
    raise RuntimeError(f"Claude APK index lacks fixed package metadata: {expected}")


def _cached_signed_claude_apk(source: Path, private_key: Path, public_key: Path) -> Path:
    payload = {
        "kind": "claude-local-signature",
        "recipe_version": RECIPE_VERSION,
        "source_sha256": sha256(source),
        "local_public_key_sha256": sha256(public_key),
    }
    destination = WORK / "claude" / fingerprint(payload)
    output = destination / CLAUDE_APK_NAME
    if manifest_matches(destination, payload, (CLAUDE_APK_NAME,)):
        return output

    generation = temporary_directory(WORK / "claude", "claude")
    published = False
    try:
        add_apk_signature(
            source,
            generation / CLAUDE_APK_NAME,
            private_key,
            public_key,
        )
        write_manifest(generation, payload)
        publish_directory(generation, destination)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    return output


def _package_identity(name: str) -> tuple[str, str]:
    stem = name.removesuffix(".apk")
    match = re.fullmatch(r"(.+?)-(\d.+-r\d+)", stem)
    if match is None:
        raise RuntimeError(f"invalid fixed APK filename: {name}")
    return match.group(1), match.group(2)


def _cached_codex_binary() -> Path:
    archive = _download(CODEX_ARCHIVE_URL, CODEX_ARCHIVE_NAME, CODEX_ARCHIVE_SHA256)
    payload = {
        "kind": "codex-native-binary",
        "recipe_version": RECIPE_VERSION,
        "version": CODEX_VERSION,
        "archive_sha256": CODEX_ARCHIVE_SHA256,
    }
    destination = WORK / "codex" / fingerprint(payload)
    if manifest_matches(destination, payload, ("codex",)):
        binary = destination / "codex"
        if binary.stat().st_mode & 0o111:
            return binary

    generation = temporary_directory(WORK / "codex", "codex")
    published = False
    try:
        with tarfile.open(archive, "r:gz") as bundle:
            members = bundle.getmembers()
            if (
                len(members) != 1
                or members[0].name != CODEX_BINARY_NAME
                or not members[0].isfile()
            ):
                raise RuntimeError(
                    f"unexpected Codex archive members: {[item.name for item in members]}"
                )
            source = bundle.extractfile(members[0])
            if source is None:
                raise RuntimeError("Codex archive lacks its native executable")
            binary = generation / "codex"
            with binary.open("wb") as stream:
                shutil.copyfileobj(source, stream)
            binary.chmod(0o755)
        write_manifest(generation, payload)
        publish_directory(generation, destination)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    return destination / "codex"


def artifact_payload(
    codex_binary: Path,
    claude_key: Path,
    claude_index: Path,
    claude_apk: Path,
    alpine_apks: tuple[Path, ...],
) -> dict[str, object]:
    """构造与全部 Guest 安装 bytes 绑定的 Agent 开发环境身份。"""
    return {
        "kind": "agent-development-assets",
        "recipe_version": RECIPE_VERSION,
        "arch": TARGET.arch,
        "codex": {
            "version": CODEX_VERSION,
            "sha256": sha256(codex_binary),
        },
        "claude": {
            "version": CLAUDE_VERSION,
            "key_sha256": sha256(claude_key),
            "index_generation": CLAUDE_INDEX_GENERATION,
            "index_sha256": sha256(claude_index),
            "source_apk_sha256": CLAUDE_APK_SHA256,
            "apk_sha256": sha256(claude_apk),
        },
        "alpine_apks": {
            archive.name: sha256(archive) for archive in alpine_apks
        },
    }


def cached_agent_cli_artifacts() -> AgentCliArtifacts:
    """返回 Agent 开发镜像需要的完整固定输入。

    Returns:
        原生 Codex、Claude repository key 与离线 APK dependency closure。

    Raises:
        RuntimeError: target、摘要、APK metadata 或 Codex archive 不符合固定契约。
        OSError: 网络、cache 或本地工具不可用。
    """
    ensure_supported_target()
    with cache_lock(WORK / ".agent-cli.lock"):
        codex_binary = _cached_codex_binary()
        claude_key = _download(
            CLAUDE_KEY_URL,
            CLAUDE_KEY_NAME,
            CLAUDE_KEY_SHA256,
        )
        claude_source_apk = _download(
            CLAUDE_APK_URL,
            CLAUDE_APK_NAME,
            CLAUDE_APK_SHA256,
        )
        _verify_apk(claude_source_apk, "claude-code", CLAUDE_VERSION)
        claude_index = _download(
            CLAUDE_INDEX_URL,
            CLAUDE_INDEX_NAME,
            CLAUDE_INDEX_SHA256,
        )
        _verify_claude_index(claude_index, claude_key)
        bootstrap = cached_apk_bootstrap()
        claude_apk = _cached_signed_claude_apk(
            claude_source_apk,
            bootstrap.private_key,
            bootstrap.public_key,
        )
        _verify_apk(claude_apk, "claude-code", CLAUDE_VERSION)

        development_apks = []
        for repository, name, digest in AGENT_ALPINE_PACKAGES:
            url = (
                f"{ALPINE_MIRROR}/{ALPINE_BRANCH}/{repository}/"
                f"{ALPINE_ARCH}/{name}"
            )
            archive = _download(url, name, digest)
            package, version = _package_identity(name)
            _verify_apk(archive, package, version)
            development_apks.append(archive)

        # Git/curl 的固定闭包已经有独立 runtime gate；Agent 开发镜像复用相同 bytes，
        # 但不安装与自举无关的 SQLite 顶层应用。
        application_apks = tuple(
            archive
            for archive in cached_application_apks().archives
            if not archive.name.startswith("sqlite-")
        )
        alpine_apks = (*application_apks, *development_apks)
        payload = artifact_payload(
            codex_binary,
            claude_key,
            claude_index,
            claude_apk,
            alpine_apks,
        )
        return AgentCliArtifacts(
            codex_binary=codex_binary,
            claude_key=claude_key,
            claude_index=claude_index,
            claude_apk=claude_apk,
            alpine_apks=alpine_apks,
            fingerprint=fingerprint(payload),
        )
