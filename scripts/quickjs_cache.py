#!/usr/bin/env python3
"""构建固定、无 quickjs-libc capability 的 target QuickJS 静态库。"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tarfile
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from build_cache import (
    build_environment,
    fingerprint,
    manifest_matches,
    publish_directory,
    sha256,
    temporary_directory,
    write_manifest,
)
from verify_musl import MuslCachePaths

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target/quickjs-runtime"
VERSION = "2026-06-04"
ARCHIVE_NAME = f"quickjs-{VERSION}.tar.xz"
ARCHIVE_URL = f"https://bellard.org/quickjs/{ARCHIVE_NAME}"
ARCHIVE_SHA256 = "b376e839b322978313d929fd20663b11ba58b75df5a46c126dd19ea2fa70ad2a"
RECIPE_VERSION = 1
SOURCES = ("quickjs.c", "dtoa.c", "libregexp.c", "libunicode.c", "cutils.c")


@dataclass(frozen=True)
class QuickJsPaths:
    """@description 已校验、无 OS standard library 的 target QuickJS 构建产物。"""

    library: Path
    include: Path
    build_id: str
    fingerprint: str


def build_quickjs_bridge(
    musl: MuslCachePaths,
    quickjs: QuickJsPaths,
    source: Path,
) -> Path:
    """构建 Rust host 与 upstream QuickJS 宏 ABI 之间的唯一窄 C bridge。"""
    payload = {
        "kind": "liteui-quickjs-bridge",
        "recipe_version": 1,
        "quickjs_fingerprint": quickjs.fingerprint,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "driver_sha256": sha256(ROOT / "scripts/musl_clang.py"),
        "source_sha256": sha256(source),
    }
    identity = fingerprint(payload)
    entry = WORK / "bridges" / identity
    if manifest_matches(entry, payload, ("liblitejs-bridge.a",)):
        return entry / "liblitejs-bridge.a"
    temporary = temporary_directory(WORK / "bridges", "bridge")
    environment = build_environment()
    environment.update(
        {
            "LITEOS_MUSL_CLANG": str(musl.compiler),
            "LITEOS_MUSL_LLD": str(musl.linker),
            "LITEOS_MUSL_LIBGCC": str(musl.libgcc),
            "LITEOS_MUSL_SYSROOT": str(musl.install),
        }
    )
    try:
        output = temporary / "bridge.o"
        _run(
            [
                sys.executable,
                str(ROOT / "scripts/musl_clang.py"),
                "-c",
                str(source),
                "-std=c11",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-Wno-unused-parameter",
                "-I",
                str(quickjs.include),
                "-o",
                str(output),
            ],
            env=environment,
        )
        library = temporary / "liblitejs-bridge.a"
        _run([_archiver(), "rcsD", str(library), str(output)])
        write_manifest(temporary, payload)
        publish_directory(temporary, entry)
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    return entry / "liblitejs-bridge.a"


def _run(command: list[str], cwd: Path = ROOT, env: dict[str, str] | None = None) -> str:
    result = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-100:])
        raise RuntimeError(f"command failed: {' '.join(command)}\n{tail}")
    return result.stdout


def _download() -> Path:
    archives = WORK / "archives"
    archives.mkdir(parents=True, exist_ok=True)
    archive = archives / ARCHIVE_NAME
    if archive.is_file() and sha256(archive) == ARCHIVE_SHA256:
        return archive
    archive.unlink(missing_ok=True)
    temporary = archive.with_suffix(".download")
    temporary.unlink(missing_ok=True)
    try:
        urllib.request.urlretrieve(ARCHIVE_URL, temporary)
    except Exception as error:
        temporary.unlink(missing_ok=True)
        raise RuntimeError(f"failed to download fixed QuickJS {VERSION}: {error}") from error
    if sha256(temporary) != ARCHIVE_SHA256:
        temporary.unlink(missing_ok=True)
        raise RuntimeError(f"QuickJS archive SHA-256 mismatch: {VERSION}")
    os.replace(temporary, archive)
    return archive


def _extract(archive: Path, destination: Path) -> Path:
    with tarfile.open(archive, "r:xz") as source:
        members = source.getmembers()
        prefix = f"quickjs-{VERSION}/"
        if not members or any(
            member.name != prefix.rstrip("/") and not member.name.startswith(prefix)
            for member in members
        ):
            raise RuntimeError("QuickJS archive contains an unexpected path")
        source.extractall(destination, filter="data")
    extracted = destination / f"quickjs-{VERSION}"
    required = (*SOURCES, "quickjs.h", "quickjs-atom.h", "quickjs-opcode.h")
    if any(not (extracted / path).is_file() for path in required):
        raise RuntimeError("QuickJS archive lacks required engine sources")
    return extracted


def _archiver() -> str:
    for candidate in ("llvm-ar", "ar"):
        path = shutil.which(candidate)
        if path is not None:
            return path
    raise RuntimeError("llvm-ar or ar is required to build QuickJS")


def build_quickjs(musl: MuslCachePaths) -> QuickJsPaths:
    """交叉构建仅含 engine 的 QuickJS；禁止 quickjs-libc/OS capability。"""
    archive = _download()
    payload = {
        "kind": "quickjs-engine",
        "recipe_version": RECIPE_VERSION,
        "version": VERSION,
        "archive_sha256": sha256(archive),
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "driver_sha256": sha256(ROOT / "scripts/musl_clang.py"),
        "sources": SOURCES,
    }
    identity = fingerprint(payload)
    entry = WORK / "engines" / identity
    required = ("lib/libquickjs.a", "include/quickjs.h")
    if manifest_matches(entry, payload, required):
        return QuickJsPaths(
            library=entry / required[0],
            include=entry / "include",
            build_id=VERSION,
            fingerprint=identity,
        )

    temporary = temporary_directory(WORK / "engines", "quickjs")
    source = _extract(archive, temporary / "source")
    objects = temporary / "objects"
    objects.mkdir()
    environment = build_environment()
    environment.update(
        {
            "LITEOS_MUSL_CLANG": str(musl.compiler),
            "LITEOS_MUSL_LLD": str(musl.linker),
            "LITEOS_MUSL_LIBGCC": str(musl.libgcc),
            "LITEOS_MUSL_SYSROOT": str(musl.install),
        }
    )
    try:
        compiled: list[Path] = []
        for name in SOURCES:
            output = objects / f"{Path(name).stem}.o"
            _run(
                [
                    sys.executable,
                    str(ROOT / "scripts/musl_clang.py"),
                    "-c",
                    str(source / name),
                    "-std=c11",
                    "-D_GNU_SOURCE",
                    f'-DCONFIG_VERSION="{VERSION}"',
                    "-O2",
                    "-fPIC",
                    "-fwrapv",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                    "-Wno-array-bounds",
                    "-Wno-format-truncation",
                    "-Wno-infinite-recursion",
                    "-Wno-sign-compare",
                    "-Wno-unused-parameter",
                    "-I",
                    str(source),
                    "-o",
                    str(output),
                ],
                env=environment,
            )
            compiled.append(output)
        library = temporary / "lib/libquickjs.a"
        library.parent.mkdir()
        _run([_archiver(), "rcsD", str(library), *(str(path) for path in compiled)])
        include = temporary / "include"
        include.mkdir()
        for name in ("quickjs.h", "quickjs-atom.h", "quickjs-opcode.h", "cutils.h"):
            shutil.copy2(source / name, include / name)
        write_manifest(temporary, payload)
        publish_directory(temporary, entry)
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    return QuickJsPaths(
        library=entry / required[0],
        include=entry / "include",
        build_id=VERSION,
        fingerprint=identity,
    )
