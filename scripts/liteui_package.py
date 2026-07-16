#!/usr/bin/env python3
"""把 LiteUI source bundle 编译成无脚本的标准 Alpine APK。"""

from __future__ import annotations

import hashlib
import tempfile
from pathlib import Path

from apk_cache import ApkBootstrapPaths
from apk_package import ApkPackageMetadata, build_signed_apk
from solidjs_cache import solid_bundle

ROOT = Path(__file__).resolve().parent.parent
PROJECT_URL = "https://github.com/lite-os/lite-os"


def _cbor_uint(value: int) -> bytes:
    if value < 24:
        return bytes((value,))
    if value <= 0xFF:
        return b"\x18" + value.to_bytes(1, "big")
    if value <= 0xFFFF:
        return b"\x19" + value.to_bytes(2, "big")
    if value <= 0xFFFFFFFF:
        return b"\x1a" + value.to_bytes(4, "big")
    return b"\x1b" + value.to_bytes(8, "big")


def _cbor_bytes(value: bytes) -> bytes:
    length = _cbor_uint(len(value))
    return bytes((length[0] | 0x40,)) + length[1:] + value


def _cbor_text(value: str) -> bytes:
    encoded = value.encode()
    length = _cbor_uint(len(encoded))
    return bytes((length[0] | 0x60,)) + length[1:] + encoded


def _manifest(
    source: bytes,
    styles: bytes,
    application_id: str,
    role: str,
    heap_limit: int,
) -> bytes:
    bundle_digest = hashlib.sha256(
        hashlib.sha256(source).digest() + hashlib.sha256(styles).digest()
    ).digest()
    values: dict[str, bytes] = {
        "abi": _cbor_uint(1),
        "entry": _cbor_text("app.mjs"),
        "heap": _cbor_uint(heap_limit),
        "id": _cbor_text(application_id),
        "role": _cbor_text(role),
        "bundle-sha256": _cbor_bytes(bundle_digest),
        "styles": _cbor_text("styles.bin"),
    }
    entries = [(_cbor_text(key), value) for key, value in values.items()]
    entries.sort(key=lambda item: (len(item[0]), item[0]))
    header = _cbor_uint(len(entries))
    return bytes((header[0] | 0xA0,)) + header[1:] + b"".join(
        key + value for key, value in entries
    )


def _styles(source: bytes) -> bytes:
    """首期保留 CSS source digest；后续 compiler 在同一 header 后追加 typed rules。"""
    return b"LSTY\x00\x01\x00\x00" + hashlib.sha256(source).digest()


def build_liteui_apk(
    bootstrap: ApkBootstrapPaths,
    output_directory: Path,
    local_name: str,
    application_id: str,
    role: str,
    heap_limit: int,
    description: str,
) -> Path:
    """构建 source-authoritative、无 install/trigger script 的标准 LiteUI APK。"""
    if not local_name or any(
        not (character.islower() or character.isdigit() or character == "-")
        for character in local_name
    ):
        raise ValueError(f"invalid LiteUI package local name: {local_name}")
    source_root = ROOT / "user/apps" / local_name / "src"
    runtime = ROOT / "user/apps/runtime/app-runtime.mjs"
    application = (
        solid_bundle()
        + b"\n"
        + runtime.read_bytes()
        + b"\n"
        + (source_root / "app.mjs").read_bytes()
    )
    styles = (source_root / "styles.css").read_bytes()
    output_directory.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=f"liteui-{local_name}-") as directory:
        root = Path(directory)
        destination = root / "usr/lib/liteui/apps" / local_name
        destination.mkdir(parents=True)
        compiled_styles = _styles(styles)
        (destination / "app.mjs").write_bytes(application)
        (destination / "manifest.cbor").write_bytes(
            _manifest(application, compiled_styles, application_id, role, heap_limit)
        )
        (destination / "styles.bin").write_bytes(compiled_styles)
        archive = output_directory / f"liteui-{local_name}-0.1.0-r0.apk"
        build_signed_apk(
            root,
            archive,
            ApkPackageMetadata(
                name=f"liteui-{local_name}",
                version="0.1.0-r0",
                description=description,
                url=PROJECT_URL,
                license="MIT",
                arch="riscv64",
                dependencies=("liteos-base",),
            ),
            bootstrap.private_key,
            bootstrap.public_key,
        )
    return archive


def build_system_shell_apk(
    bootstrap: ApkBootstrapPaths,
    output_directory: Path,
) -> Path:
    """构建受信、8 MiB heap 的 System Shell APK。"""
    return build_liteui_apk(
        bootstrap,
        output_directory,
        "system-shell",
        "org.liteos.system-shell",
        "system-shell",
        8 * 1024 * 1024,
        "LiteOS QuickJS Solid System Shell",
    )


def build_calculator_apk(
    bootstrap: ApkBootstrapPaths,
    output_directory: Path,
) -> Path:
    """构建无特权、4 MiB heap 的 Calculator APK。"""
    return build_liteui_apk(
        bootstrap,
        output_directory,
        "calculator",
        "org.liteos.calculator",
        "application",
        4 * 1024 * 1024,
        "LiteOS Solid Calculator",
    )
