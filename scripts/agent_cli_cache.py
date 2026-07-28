#!/usr/bin/env python3
"""获取 LiteOS AArch64 Agent 开发环境的固定 npm cache 与 APK 输入。"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import tarfile
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from apk_apps_cache import cached_application_apks
from apk_cache import ALPINE_ARCH, ALPINE_BRANCH, ALPINE_MIRROR
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

NPM_REGISTRY = "https://registry.npmjs.org"
CODEX_PACKAGE = "@openai/codex"
CODEX_VERSION = "0.145.0"
CODEX_PLATFORM_VERSION = f"{CODEX_VERSION}-linux-arm64"
CLAUDE_PACKAGE = "@anthropic-ai/claude-code"
CLAUDE_VERSION = "2.1.212"

# npm registry 的 SRI 是 package version 的不可变内容身份。构建 cache 前逐项与官方 registry
# metadata 核对；缺少该检查时，同名版本的错误 mirror 响应会进入持久开发镜像。
NPM_PACKAGE_INTEGRITIES = (
    (
        CODEX_PACKAGE,
        CODEX_VERSION,
        "sha512-/PSPSFujjjmiyVFvG2yu/grOFhsWdokTH8t2KGWhXSo/M5n/dIDsnbsnO82/7bLtIoDuzQf7ATBUMWqPWQINlQ==",
    ),
    (
        CODEX_PACKAGE,
        CODEX_PLATFORM_VERSION,
        "sha512-8OLcPXaAol/FOrRoDxWhIiHIFa73KRsM41EKocjRZOwiT4TcelzJWn3dHyiuSb7teWF25rrslvSPyvhULYRRCQ==",
    ),
    (
        CLAUDE_PACKAGE,
        CLAUDE_VERSION,
        "sha512-MEasj1oaoARRKEWU7eHJ6DWC2TC8ogml9QUDihbmxYI2Ij5Ol1leW90DIj8/a0xX3lfHZOwT3gJr0JxVKa8Sxw==",
    ),
    (
        f"{CLAUDE_PACKAGE}-linux-arm64-musl",
        CLAUDE_VERSION,
        "sha512-OmNXhGKaf1F3XrqYL5GnMIAFMv4Og3H4ehEREX6JLiZU2AC3ckyPawqvvhqyhoJx+a6KN59+6rEC97DyQMgo5Q==",
    ),
)

# Node/npm 及其 ELF dependency closure 与 Agent 常用 shell/tooling。全部文件名和摘要固定，
# Guest 只执行离线 apk transaction，产品 rootfs 不继承这些开发包。
AGENT_ALPINE_PACKAGES = (
    ("main", "ada-libs-2.9.2-r4.apk", "58147891c4ae32752fd81792dfec19c71b8d88661c4aa30db3f26600df33bb28"),
    ("main", "bash-5.2.37-r0.apk", "411e1fec2dccd603bc9f23586f7b8df2211613ece49b20c71c17412ab2667c44"),
    ("main", "ca-certificates-20260611-r0.apk", "6b491dcda951129c80e8d7b0f509253ab640b20653b208d3b0994d893189b3f5"),
    ("main", "icu-data-en-76.1-r1.apk", "2c2d36d47c82d0f6cff1b549044fe3562f327f944971ef367b9c40eeb35aa6e8"),
    ("main", "icu-libs-76.1-r1.apk", "6c9dd2e6b0ddc6e7d5fd2a21b427799d7ca4f7e8b5aad72d17e84520db3cd249"),
    ("main", "libgcc-14.2.0-r6.apk", "ba1835eec3ad8a120efd3d5020e561d53553a0513763a08f509e3ce6d4baa9ca"),
    ("main", "libstdc++-14.2.0-r6.apk", "0d2f054057a4f932e985a129eccb79908b40964185139a0a609aed3032aba064"),
    ("main", "nodejs-22.23.0-r0.apk", "8320f5e9cd6d37225d19a8fa66e437589c14300bc0386841b7f88ec44b74da20"),
    ("community", "npm-11.6.4-r0.apk", "0ec0386135848268c5d316b2f28f2cbac7084686df20919e727942914f74cbfe"),
    ("community", "ripgrep-14.1.1-r0.apk", "f9c145aca9868a3a90d57d4eb89a4c1c92bc4f06870311d230856f68cf6e58bd"),
    ("main", "simdjson-3.12.0-r0.apk", "5605c691ab62e5a0071d065b5afdd5c3740d763821d689a6bf54e46c95916974"),
    ("main", "simdutf-7.2.1-r0.apk", "b20688ad72d096ba903bc77ea92165153427dcf5d25b5b9dcf27e7b4ca7046b9"),
    ("main", "sqlite-libs-3.49.2-r1.apk", "204910bcbb13df4d517cb01acb178ebe14f12ff0e55a04b38d1565941780ee29"),
)
RECIPE_VERSION = 2


@dataclass(frozen=True)
class AgentCliArtifacts:
    """已校验并缓存的 npm 离线输入与 Alpine dependency closure。"""

    npm_cache_archive: Path
    alpine_apks: tuple[Path, ...]
    fingerprint: str


def ensure_supported_target() -> None:
    """确认当前 target 能消费固定的 AArch64 npm/Alpine 产物。

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
    """下载一个固定 APK，并只在摘要匹配后发布到共享 cache。"""
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


def _package_identity(name: str) -> tuple[str, str]:
    stem = name.removesuffix(".apk")
    match = re.fullmatch(r"(.+?)-(\d.+-r\d+)", stem)
    if match is None:
        raise RuntimeError(f"invalid fixed APK filename: {name}")
    return match.group(1), match.group(2)


def _verify_apk(archive: Path, package: str, version: str) -> None:
    """验证固定 APK 的 package/version 与 AArch64/noarch metadata。"""
    metadata: dict[str, str] = {}
    for line in _run(["tar", "-xOf", str(archive), ".PKGINFO"]).splitlines():
        key, separator, value = line.partition(" = ")
        if separator and key in {"pkgname", "pkgver", "arch"}:
            metadata[key] = value
    if (
        metadata.get("pkgname") != package
        or metadata.get("pkgver") != version
        or metadata.get("arch") not in {"aarch64", "noarch"}
    ):
        raise RuntimeError(
            f"Agent APK metadata mismatch: {archive.name}; actual={metadata}"
        )


def _registry_integrity(package: str, version: str, cache: Path) -> str:
    command = [
        "npm",
        "view",
        "--registry",
        NPM_REGISTRY,
        "--cache",
        str(cache),
        f"{package}@{version}",
        "dist.integrity",
        "--json",
    ]
    result = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join((result.stdout + result.stderr).splitlines()[-60:])
        raise RuntimeError(f"command failed: {' '.join(command)}\n{tail}")
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(
            f"npm returned invalid integrity metadata: {result.stdout}"
        ) from error
    if not isinstance(value, str):
        raise RuntimeError(f"npm integrity metadata is not a string: {value!r}")
    return value


def _verify_npm_prefix(prefix: Path) -> None:
    expected = {
        prefix / "lib/node_modules/@openai/codex/package.json": (
            CODEX_PACKAGE,
            CODEX_VERSION,
        ),
        prefix
        / "lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-arm64/package.json": (
            CODEX_PACKAGE,
            CODEX_PLATFORM_VERSION,
        ),
        prefix / "lib/node_modules/@anthropic-ai/claude-code/package.json": (
            CLAUDE_PACKAGE,
            CLAUDE_VERSION,
        ),
        prefix
        / "lib/node_modules/@anthropic-ai/claude-code/node_modules/@anthropic-ai/claude-code-linux-arm64-musl/package.json": (
            f"{CLAUDE_PACKAGE}-linux-arm64-musl",
            CLAUDE_VERSION,
        ),
    }
    for path, identity in expected.items():
        metadata = json.loads(path.read_text())
        if (metadata.get("name"), metadata.get("version")) != identity:
            raise RuntimeError(f"npm installed unexpected package metadata: {path}")
    required = (
        prefix
        / "lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-arm64/vendor/aarch64-unknown-linux-musl/bin/codex",
        prefix
        / "lib/node_modules/@anthropic-ai/claude-code/node_modules/@anthropic-ai/claude-code-linux-arm64-musl/claude",
    )
    if not all(path.is_file() and path.stat().st_mode & 0o111 for path in required):
        raise RuntimeError("npm cache validation lacks an executable AArch64 musl CLI")


def _archive_cache(cache: Path, output: Path) -> None:
    """把 npm cacache 打包为 metadata 归一化、Guest 可离线展开的 tar。"""
    with tarfile.open(output, "w") as archive:
        for path in sorted(cache.rglob("*")):
            relative = Path("npm-cache") / path.relative_to(cache)
            info = archive.gettarinfo(str(path), str(relative))
            info.uid = 0
            info.gid = 0
            info.uname = "root"
            info.gname = "root"
            info.mtime = 0
            if path.is_file():
                with path.open("rb") as stream:
                    archive.addfile(info, stream)
            else:
                archive.addfile(info)


def _cached_npm_cache_archive() -> Path:
    payload = {
        "kind": "agent-npm-cache",
        "recipe_version": RECIPE_VERSION,
        "registry": NPM_REGISTRY,
        "packages": {
            f"{package}@{version}": integrity
            for package, version, integrity in NPM_PACKAGE_INTEGRITIES
        },
    }
    destination = WORK / "npm-cache" / fingerprint(payload)
    output = destination / "npm-cache.tar"
    if manifest_matches(destination, payload, ("npm-cache.tar",)):
        return output

    generation = temporary_directory(WORK / "npm-cache", "npm-cache")
    published = False
    try:
        cache = generation / "npm-cache"
        prefix = generation / "prefix"
        cache.mkdir()
        prefix.mkdir()
        for package, version, expected_integrity in NPM_PACKAGE_INTEGRITIES:
            actual = _registry_integrity(package, version, cache)
            if actual != expected_integrity:
                raise RuntimeError(
                    f"npm integrity mismatch for {package}@{version}: "
                    f"expected={expected_integrity}, actual={actual}"
                )
            _run(
                [
                    "npm",
                    "cache",
                    "add",
                    "--registry",
                    NPM_REGISTRY,
                    "--cache",
                    str(cache),
                    f"{package}@{version}",
                ]
            )
        _run(
            [
                "npm",
                "install",
                "--global",
                "--prefix",
                str(prefix),
                "--cache",
                str(cache),
                "--offline",
                "--registry",
                NPM_REGISTRY,
                "--os=linux",
                "--cpu=arm64",
                "--libc=musl",
                "--include=optional",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                f"{CODEX_PACKAGE}@{CODEX_VERSION}",
                f"{CLAUDE_PACKAGE}@{CLAUDE_VERSION}",
            ]
        )
        _verify_npm_prefix(prefix)
        shutil.rmtree(cache / "_logs", ignore_errors=True)
        (cache / "_update-notifier-last-checked").unlink(missing_ok=True)
        _archive_cache(cache, generation / "npm-cache.tar")
        shutil.rmtree(cache)
        shutil.rmtree(prefix)
        write_manifest(generation, payload)
        publish_directory(generation, destination)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    return output


def artifact_payload(artifacts: AgentCliArtifacts) -> dict[str, object]:
    """构造与全部 Guest npm/APK 安装 bytes 绑定的开发环境身份。"""
    return {
        "kind": "agent-development-assets",
        "recipe_version": RECIPE_VERSION,
        "arch": TARGET.arch,
        "npm_cache_sha256": sha256(artifacts.npm_cache_archive),
        "npm_packages": {
            f"{package}@{version}": integrity
            for package, version, integrity in NPM_PACKAGE_INTEGRITIES
        },
        "alpine_apks": {
            archive.name: sha256(archive) for archive in artifacts.alpine_apks
        },
    }


def cached_agent_cli_artifacts() -> AgentCliArtifacts:
    """返回 Agent 开发镜像需要的完整固定输入。

    Returns:
        npm 离线 cache 与 Node、npm、Git、curl、shell 的 Alpine APK 闭包。

    Raises:
        RuntimeError: target、npm SRI、APK 摘要或 package metadata 不符合固定契约。
        OSError: 网络、cache 或本地工具不可用。
    """
    ensure_supported_target()
    with cache_lock(WORK / ".agent-cli.lock"):
        npm_cache_archive = _cached_npm_cache_archive()
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

        # Git/curl 的固定闭包已有独立 runtime gate；Agent 开发镜像复用相同 bytes，
        # 但不安装与自举无关的 SQLite 顶层应用。
        application_apks = tuple(
            archive
            for archive in cached_application_apks().archives
            if not archive.name.startswith("sqlite-")
        )
        alpine_apks = (*application_apks, *development_apks)
        provisional = AgentCliArtifacts(
            npm_cache_archive=npm_cache_archive,
            alpine_apks=alpine_apks,
            fingerprint="",
        )
        payload = artifact_payload(provisional)
        return AgentCliArtifacts(
            npm_cache_archive=npm_cache_archive,
            alpine_apks=alpine_apks,
            fingerprint=fingerprint(payload),
        )
