#!/usr/bin/env python3
"""获取固定 Solid universal renderer，并生成无 module import 的 QuickJS bundle。"""

from __future__ import annotations

import os
import tarfile
import urllib.request
from pathlib import Path

from build_cache import sha256

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target/solidjs-runtime"
VERSION = "1.9.14"
ARCHIVE_NAME = f"solid-js-{VERSION}.tgz"
ARCHIVE_URL = f"https://registry.npmjs.org/solid-js/-/{ARCHIVE_NAME}"
ARCHIVE_SHA256 = "0aae69da57139bb29fc6f4b0055a8a1804e8eafbef26dd1099c3039fd5f7e0f5"
CORE = "package/dist/solid.js"
UNIVERSAL = "package/universal/dist/universal.js"
LICENSE = "package/LICENSE"


def solid_bundle() -> bytes:
    """返回固定 core + universal renderer；应用源码在其后直接消费 lexical exports。"""
    archive = _download()
    with tarfile.open(archive, "r:gz") as source:
        names = set(source.getnames())
        if not {CORE, UNIVERSAL, LICENSE}.issubset(names):
            raise RuntimeError("SolidJS archive lacks required universal renderer files")
        core = _member(source, CORE)
        universal = _member(source, UNIVERSAL)
        license_text = _member(source, LICENSE)
    export = core.rfind(b"\nexport {")
    if export < 0:
        raise RuntimeError("SolidJS core export boundary changed")
    import_line = universal.find(b"\n")
    universal_export = universal.rfind(b"\nexport {")
    if not universal.startswith(b"import {") or import_line < 0 or universal_export < 0:
        raise RuntimeError("SolidJS universal module boundary changed")
    notice = b"\n/* SolidJS " + VERSION.encode() + b" license:\n" + license_text + b"\n*/\n"
    return core[:export] + b"\n" + universal[import_line + 1 : universal_export] + notice


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
        raise RuntimeError(f"failed to download fixed SolidJS {VERSION}: {error}") from error
    if sha256(temporary) != ARCHIVE_SHA256:
        temporary.unlink(missing_ok=True)
        raise RuntimeError(f"SolidJS archive SHA-256 mismatch: {VERSION}")
    os.replace(temporary, archive)
    return archive


def _member(source: tarfile.TarFile, name: str) -> bytes:
    member = source.getmember(name)
    if not member.isfile() or member.size > 1024 * 1024:
        raise RuntimeError(f"SolidJS archive member is not bounded: {name}")
    extracted = source.extractfile(member)
    if extracted is None:
        raise RuntimeError(f"SolidJS archive member is unreadable: {name}")
    return extracted.read()
