#!/usr/bin/env python3
"""构建固定 OpenSSL LTS HTTPS helper，并缓存 Mozilla CA trust bundle。"""

from __future__ import annotations

import shutil
import sys
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from build_cache import (
    build_environment,
    fingerprint,
    generation_directory,
    make_command,
    manifest_matches,
    publish_directory,
    publish_generation,
    sha256,
    temporary_directory,
    write_manifest,
)
from verify_musl import MuslCachePaths, compiler_identity, run

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target/openssl-runtime"
OPENSSL_VERSION = "3.5.7"
OPENSSL_URL = (
    "https://github.com/openssl/openssl/releases/download/"
    f"openssl-{OPENSSL_VERSION}/openssl-{OPENSSL_VERSION}.tar.gz"
)
OPENSSL_SHA256 = "a8c0d28a529ca480f9f36cf5792e2cd21984552a3c8e4aa11a24aa31aeac98e8"
CA_BUNDLE_VERSION = "2026-05-14"
CA_BUNDLE_URL = f"https://curl.se/ca/cacert-{CA_BUNDLE_VERSION}.pem"
CA_BUNDLE_SHA256 = "86a1f3366afac7c6f8ae9f3c779ac221129328c43f0ab2b8817eb2f362a5025c"
CONFIGURE_OPTIONS = (
    "no-shared",
    "no-tests",
    "no-module",
    "no-dso",
    "no-legacy",
    "no-comp",
)


@dataclass(frozen=True)
class OpenSslPaths:
    binary: Path
    ca_bundle: Path
    fingerprint: str


def _download(url: str, destination: Path, expected_sha256: str, label: str) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.is_file() and sha256(destination) == expected_sha256:
        return
    destination.unlink(missing_ok=True)
    temporary = destination.with_suffix(destination.suffix + ".download")
    temporary.unlink(missing_ok=True)
    print(f"downloading {label}")
    try:
        urllib.request.urlretrieve(url, temporary)
    except Exception as error:
        temporary.unlink(missing_ok=True)
        raise RuntimeError(f"failed to download {url}: {error}") from error
    if sha256(temporary) != expected_sha256:
        temporary.unlink(missing_ok=True)
        raise RuntimeError(f"{label} SHA-256 mismatch")
    temporary.replace(destination)


def _source() -> tuple[Path, str]:
    payload = {
        "kind": "openssl-source",
        "version": OPENSSL_VERSION,
        "archive_sha256": OPENSSL_SHA256,
        "strip_components": 1,
    }
    source_fingerprint = fingerprint(payload)
    archive = WORK / f"openssl-{OPENSSL_VERSION}.tar.gz"
    _download(OPENSSL_URL, archive, OPENSSL_SHA256, f"OpenSSL {OPENSSL_VERSION}")
    source = WORK / "sources" / source_fingerprint
    if manifest_matches(source, payload, ("Configure", "VERSION.dat")):
        return source, source_fingerprint
    temporary = temporary_directory(WORK / "sources", "source")
    try:
        run(
            ["tar", "-xzf", str(archive), "--strip-components=1", "-C", str(temporary)],
            ROOT,
        )
        write_manifest(temporary, payload)
        publish_directory(temporary, source)
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    return source, source_fingerprint


def _ca_bundle() -> Path:
    bundle = WORK / f"cacert-{CA_BUNDLE_VERSION}.pem"
    _download(CA_BUNDLE_URL, bundle, CA_BUNDLE_SHA256, "Mozilla CA bundle")
    return bundle


def build_openssl(
    musl: MuslCachePaths,
    jobs_override: int | None,
    rebuild: bool = False,
) -> OpenSslPaths:
    source, source_fingerprint = _source()
    ca_bundle = _ca_bundle()
    payload = {
        "kind": "openssl-static-libs-dynamic-musl-cli",
        "recipe_version": 1,
        "source_fingerprint": source_fingerprint,
        "ca_bundle_sha256": CA_BUNDLE_SHA256,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "compiler": compiler_identity(musl.compiler),
        "driver_sha256": sha256(ROOT / "scripts/musl_clang.py"),
        "configure_target": "linux64-riscv64",
        "configure_options": list(CONFIGURE_OPTIONS),
    }
    entry_fingerprint = fingerprint(payload)
    entry = WORK / "binaries" / entry_fingerprint
    if not rebuild and manifest_matches(entry, payload, ("openssl",)):
        print(f"OpenSSL binary cache hit: {entry_fingerprint[:12]}")
        return OpenSslPaths(entry / "openssl", ca_bundle, entry_fingerprint)

    build = temporary_directory(WORK / "builds", "build")
    generation = generation_directory(WORK / "binary-generations", entry_fingerprint)
    env = build_environment()
    env.update(
        {
            "LITEOS_MUSL_CLANG": str(musl.compiler),
            "LITEOS_MUSL_LLD": str(musl.linker),
            "LITEOS_MUSL_LIBGCC": str(musl.libgcc),
            "LITEOS_MUSL_SYSROOT": str(musl.install),
            "CC": f"{sys.executable} {ROOT / 'scripts/musl_clang.py'}",
            "AR": str(musl.archiver),
            "RANLIB": str(Path(musl.archiver).with_name("llvm-ranlib")),
        }
    )
    published = False
    try:
        run(
            [
                str(source / "Configure"),
                "linux64-riscv64",
                "--prefix=/usr",
                "--openssldir=/etc/ssl",
                *CONFIGURE_OPTIONS,
            ],
            build,
            env,
        )
        run([*make_command(jobs_override), "build_programs"], build, env)
        built = build / "apps/openssl"
        if not built.is_file():
            raise RuntimeError("OpenSSL build did not produce apps/openssl")
        run(
            [
                "/opt/homebrew/opt/llvm/bin/llvm-strip",
                "-o",
                str(generation / "openssl"),
                str(built),
            ],
            ROOT,
        )
        write_manifest(generation, payload)
        publish_generation(generation, entry)
        published = True
    finally:
        shutil.rmtree(build, ignore_errors=True)
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    print(f"OpenSSL binary cache populated: {entry_fingerprint[:12]}")
    return OpenSslPaths(entry / "openssl", ca_bundle, entry_fingerprint)
