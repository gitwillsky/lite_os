#!/usr/bin/env python3
"""构建并缓存固定 upstream libseat 与 libdrm userspace display stack。"""

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
    manifest_matches,
    publish_directory,
    publish_generation,
    sha256,
    temporary_directory,
    write_manifest,
)
from verify_musl import MuslCachePaths, run

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target/display-stack"
SEATD_VERSION = "0.9.3"
SEATD_REVISION = "daa8196e10b180b8b0caeafa8e5f860eb1bd6706"
SEATD_URL = f"https://git.sr.ht/~kennylevinsen/seatd/archive/{SEATD_VERSION}.tar.gz"
SEATD_SHA256 = "302564d54d8e28191fadfd734f2675ecb0c9e0615a58011b89ef15dfa4dbaa96"
LIBDRM_VERSION = "2.4.134"
LIBDRM_REVISION = "e984d448b8b17aab853369e6c203e53719f46de1"
LIBDRM_URL = (
    "https://gitlab.freedesktop.org/mesa/drm/-/archive/"
    f"libdrm-{LIBDRM_VERSION}/drm-libdrm-{LIBDRM_VERSION}.tar.gz"
)
LIBDRM_SHA256 = "6b18e4834b0c061232cb5c11e98a6ecdc72ebc6bc282d124406b7a9d4e089ce2"


@dataclass(frozen=True)
class DisplayStackPaths:
    """固定 display libraries 与 headers 的 content-addressed install tree。"""

    install: Path
    libseat: Path
    libdrm: Path
    fingerprint: str


def obtain_source(name: str, url: str, digest: str, marker: str) -> Path:
    """下载、校验并原子发布一份固定 upstream source tree。"""
    archive = WORK / "archives" / f"{name}.tar.gz"
    archive.parent.mkdir(parents=True, exist_ok=True)
    if not archive.is_file() or sha256(archive) != digest:
        archive.unlink(missing_ok=True)
        temporary = archive.with_suffix(".download")
        temporary.unlink(missing_ok=True)
        urllib.request.urlretrieve(url, temporary)
        if sha256(temporary) != digest:
            temporary.unlink(missing_ok=True)
            raise RuntimeError(f"{name} release tarball SHA-256 mismatch")
        temporary.replace(archive)
    payload = {"kind": f"{name}-source", "sha256": digest, "strip_components": 1}
    source = WORK / "sources" / fingerprint(payload)
    if manifest_matches(source, payload, (marker,)):
        return source
    temporary = temporary_directory(WORK / "sources", name)
    try:
        run(
            ["tar", "-xzf", str(archive), "--strip-components=1", "-C", str(temporary)],
            ROOT,
        )
        write_manifest(temporary, payload)
        publish_directory(temporary, source)
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    return source


def build_display_stack(musl: MuslCachePaths) -> DisplayStackPaths:
    """以固定 cross file 构建 seatd backend-only libseat 与 core-only libdrm。"""
    meson = shutil.which("meson")
    ninja = shutil.which("ninja")
    strip = shutil.which("llvm-strip")
    if meson is None or ninja is None or strip is None:
        raise RuntimeError("meson, ninja and llvm-strip are required for display libraries")
    seatd = obtain_source(
        f"seatd-{SEATD_VERSION}", SEATD_URL, SEATD_SHA256, "include/libseat.h"
    )
    libdrm = obtain_source(
        f"libdrm-{LIBDRM_VERSION}", LIBDRM_URL, LIBDRM_SHA256, "xf86drmMode.c"
    )
    payload = {
        "kind": "display-stack",
        "recipe_version": 3,
        "seatd": {"version": SEATD_VERSION, "revision": SEATD_REVISION, "sha256": SEATD_SHA256},
        "libdrm": {"version": LIBDRM_VERSION, "revision": LIBDRM_REVISION, "sha256": LIBDRM_SHA256},
        "musl": musl.sysroot_fingerprint,
        "driver": sha256(ROOT / "scripts/musl_clang.py"),
        "meson": run([meson, "--version"], ROOT).strip(),
        "ninja": run([ninja, "--version"], ROOT).strip(),
    }
    key = fingerprint(payload)
    entry = WORK / "installs" / key
    required = ("usr/lib/libseat.so.1", "usr/lib/libdrm.so.2", "usr/include/libseat.h")
    if manifest_matches(entry, payload, required):
        return paths(entry, key)
    generation = generation_directory(WORK / "install-generations", key)
    cross_file = generation / "riscv64-linux-musl.ini"
    cross_file.write_text(cross_configuration(musl, strip))
    env = build_environment()
    env.update(
        {
            "LITEOS_MUSL_CLANG": str(musl.compiler),
            "LITEOS_MUSL_LLD": str(musl.linker),
            "LITEOS_MUSL_LIBGCC": str(musl.libgcc),
            "LITEOS_MUSL_SYSROOT": str(musl.install),
            "DESTDIR": str(generation / "root"),
        }
    )
    published = False
    try:
        configure_seatd(meson, seatd, generation, cross_file, env)
        configure_libdrm(meson, libdrm, generation, cross_file, env)
        install = generation / "root"
        run(
            [
                strip,
                "--strip-unneeded",
                str(install / "usr/lib/libseat.so.1"),
                str(install / "usr/lib/libdrm.so.2"),
            ],
            ROOT,
            env,
        )
        cross_file.unlink()
        write_manifest(install, payload)
        publish_generation(install, entry)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    return paths(entry, key)


def cross_configuration(musl: MuslCachePaths, strip: str) -> str:
    compiler = [sys.executable, str(ROOT / "scripts/musl_clang.py")]
    quoted = ", ".join(repr(value) for value in compiler)
    return (
        "[binaries]\n"
        f"c = [{quoted}]\n"
        f"ar = {str(musl.archiver)!r}\n"
        f"strip = {strip!r}\n"
        "[host_machine]\n"
        "system = 'linux'\n"
        "cpu_family = 'riscv64'\n"
        "cpu = 'riscv64'\n"
        "endian = 'little'\n"
        "[properties]\n"
        "needs_exe_wrapper = true\n"
        "[built-in options]\n"
        "c_args = ['-fPIC']\n"
        "c_link_args = ['-Wl,-z,relro,-z,now,-z,noexecstack']\n"
    )


def configure_seatd(
    meson: str, source: Path, generation: Path, cross_file: Path, env: dict[str, str]
) -> None:
    build = generation / "seatd-build"
    run(
        [
            meson, "setup", str(build), str(source), "--cross-file", str(cross_file),
            "--prefix=/usr", "--libdir=lib", "--buildtype=release", "-Dauto_features=disabled",
            "-Dlibseat-logind=disabled", "-Dlibseat-seatd=enabled", "-Dlibseat-builtin=disabled",
            "-Dserver=disabled", "-Dexamples=disabled", "-Dman-pages=disabled",
            "-Ddefaultpath=/run/seatd.sock", "-Dc_args=-Wno-sign-compare",
        ],
        ROOT,
        env,
    )
    run([meson, "compile", "-C", str(build), "seat"], ROOT, env)
    run([meson, "install", "--no-rebuild", "--strip", "-C", str(build)], ROOT, env)


def configure_libdrm(
    meson: str, source: Path, generation: Path, cross_file: Path, env: dict[str, str]
) -> None:
    build = generation / "libdrm-build"
    options = [
        "intel", "radeon", "amdgpu", "nouveau", "vmwgfx", "omap", "exynos",
        "freedreno", "tegra", "vc4", "etnaviv", "cairo-tests", "man-pages", "valgrind",
    ]
    run(
        [
            meson, "setup", str(build), str(source), "--cross-file", str(cross_file),
            "--prefix=/usr", "--libdir=lib", "--buildtype=release", "-Dauto_features=disabled",
            *(f"-D{option}=disabled" for option in options), "-Dtests=false", "-Dudev=false",
            "-Dinstall-test-programs=false", "-Dfreedreno-kgsl=false",
            "-Dc_args=-DMAJOR_IN_SYSMACROS=1",
        ],
        ROOT,
        env,
    )
    run([meson, "compile", "-C", str(build), "drm"], ROOT, env)
    run([meson, "install", "--no-rebuild", "--strip", "-C", str(build)], ROOT, env)


def paths(entry: Path, key: str) -> DisplayStackPaths:
    return DisplayStackPaths(
        install=entry,
        libseat=entry / "usr/lib/libseat.so.1",
        libdrm=entry / "usr/lib/libdrm.so.2",
        fingerprint=key,
    )
