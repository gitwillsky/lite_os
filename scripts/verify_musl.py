#!/usr/bin/env python3
"""构建固定 musl pthread consumer，并通过 ELF 与 QEMU 冷启动围栏。"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from build_cache import (
    build_environment,
    build_jobs_override,
    cache_lock,
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

from qemu_gate import boot

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target" / "musl-static"
MUSL_VERSION = "1.2.6"
MUSL_REVISION = "9fa28ece75d8a2191de7c5bb53bed224c5947417"
MUSL_URL = f"https://musl.libc.org/releases/musl-{MUSL_VERSION}.tar.gz"
MUSL_SHA256 = "d585fd3b613c66151fc3249e8ed44f77020cb5e6c1e635a616d3f9f82460512a"
SOURCE_RECIPE_VERSION = 1
SYSROOT_RECIPE_VERSION = 2
SMOKE_RECIPE_VERSION = 2
CONFIGURE_ARGUMENTS = ("--target=riscv64", "--disable-shared")
SMOKE_LINK_ARGUMENTS = (
    "-static",
    "-no-pie",
    "-nostdlib",
    "-nostartfiles",
    "-ffreestanding",
    "-fno-stack-protector",
    "-march=rv64gc",
    "-mabi=lp64d",
    "-Wl,--gc-sections",
    "-Wl,-Ttext-segment=0x10000",
)


@dataclass(frozen=True)
class MuslCachePaths:
    source: Path
    install: Path
    sysroot_fingerprint: str


def run(command: list[str], cwd: Path, env: dict[str, str] | None = None) -> str:
    """执行构建步骤；失败时只暴露足够定位问题的输出尾部。"""
    result = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-80:])
        raise RuntimeError(f"command failed: {' '.join(command)}\n{tail}")
    return result.stdout


def source_payload() -> dict[str, object]:
    return {
        "kind": "musl-source",
        "recipe_version": SOURCE_RECIPE_VERSION,
        "version": MUSL_VERSION,
        "revision": MUSL_REVISION,
        "archive_sha256": MUSL_SHA256,
        "strip_components": 1,
    }


def source_cache_path() -> Path:
    return WORK / "sources" / fingerprint(source_payload())


def obtain_source() -> Path:
    """获取并缓存固定官方源码；完整目录只在校验和解压成功后发布。"""
    archive = WORK / f"musl-{MUSL_VERSION}.tar.gz"
    if not archive.is_file() or sha256(archive) != MUSL_SHA256:
        archive.unlink(missing_ok=True)
        temporary = archive.with_suffix(".download")
        temporary.unlink(missing_ok=True)
        print(f"downloading musl {MUSL_VERSION} ({MUSL_REVISION[:12]})")
        try:
            urllib.request.urlretrieve(MUSL_URL, temporary)
        except Exception as error:
            temporary.unlink(missing_ok=True)
            raise RuntimeError(f"failed to download {MUSL_URL}: {error}") from error
        if sha256(temporary) != MUSL_SHA256:
            temporary.unlink(missing_ok=True)
            raise RuntimeError("musl release tarball SHA-256 mismatch")
        temporary.replace(archive)

    payload = source_payload()
    source = source_cache_path()
    if manifest_matches(source, payload, ("configure", "tools/musl-gcc.specs.sh")):
        return source

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
    return source


def find_compiler() -> Path:
    candidates = (
        shutil.which("riscv64-linux-gnu-gcc"),
        shutil.which("riscv64-unknown-linux-gnu-gcc"),
        shutil.which("riscv64-unknown-elf-gcc"),
        "/opt/homebrew/bin/riscv64-unknown-elf-gcc",
    )
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return Path(candidate).resolve()
    raise RuntimeError("a RISC-V GCC cross compiler is required")


def compiler_identity(compiler: Path) -> dict[str, object]:
    return {
        "path": str(compiler),
        "target": run([str(compiler), "-dumpmachine"], ROOT).strip(),
        "version": run([str(compiler), "--version"], ROOT).splitlines()[0],
    }


def sysroot_payload(compiler: Path) -> dict[str, object]:
    return {
        "kind": "musl-static-sysroot",
        "recipe_version": SYSROOT_RECIPE_VERSION,
        "source_fingerprint": fingerprint(source_payload()),
        "compiler": compiler_identity(compiler),
        "configure_arguments": list(CONFIGURE_ARGUMENTS),
        "environment": {
            "LC_ALL": "C",
            "CPATH": None,
            "C_INCLUDE_PATH": None,
            "CPLUS_INCLUDE_PATH": None,
            "LIBRARY_PATH": None,
        },
    }


def sysroot_cache_path(payload: dict[str, object]) -> Path:
    return WORK / "installs" / fingerprint(payload)


def build_musl(
    source: Path,
    compiler: Path,
    jobs_override: int | None,
    rebuild: bool = False,
) -> tuple[Path, str]:
    """按 compiler/recipe fingerprint 构建或复用静态 musl sysroot。"""
    payload = sysroot_payload(compiler)
    sysroot_fingerprint = fingerprint(payload)
    install = sysroot_cache_path(payload)
    required = ("lib/libc.a", "lib/crt1.o", "lib/crti.o", "lib/crtn.o")
    if not rebuild and manifest_matches(install, payload, required):
        print(f"musl sysroot cache hit: {sysroot_fingerprint[:12]}")
        return install, sysroot_fingerprint

    build = temporary_directory(WORK / "builds", "build")
    generation = generation_directory(WORK / "install-generations", sysroot_fingerprint)
    prefix = str(compiler)[: -len("gcc")]
    env = build_environment()
    published = False
    try:
        run(
            [
                str(source / "configure"),
                *CONFIGURE_ARGUMENTS,
                f"--prefix={generation}",
                f"CROSS_COMPILE={prefix}",
            ],
            build,
            env,
        )
        run(make_command(jobs_override), build, env)
        run(["make", "install"], build, env)
        if not all((generation / relative).is_file() for relative in required):
            raise RuntimeError("musl install is missing required static sysroot artifacts")
        write_manifest(generation, payload)
        publish_generation(generation, install)
        published = True
    finally:
        shutil.rmtree(build, ignore_errors=True)
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    print(f"musl sysroot cache populated: {sysroot_fingerprint[:12]}")
    return install, sysroot_fingerprint


def cached_musl_paths(compiler: Path) -> MuslCachePaths:
    """只返回已经完整发布且 fingerprint 匹配的 source/sysroot。"""
    source = source_cache_path()
    if not manifest_matches(source, source_payload(), ("configure", "tools/musl-gcc.specs.sh")):
        raise RuntimeError("musl source cache is missing; run verify_musl.py first")
    payload = sysroot_payload(compiler)
    install = sysroot_cache_path(payload)
    required = ("lib/libc.a", "lib/crt1.o", "lib/crti.o", "lib/crtn.o")
    if not manifest_matches(install, payload, required):
        raise RuntimeError("musl sysroot cache is missing; run verify_musl.py first")
    return MuslCachePaths(source.resolve(), install.resolve(), fingerprint(payload))


def smoke_payload(install: Path, compiler: Path, sysroot_fingerprint: str) -> dict[str, object]:
    libgcc = Path(run([str(compiler), "-print-libgcc-file-name"], ROOT).strip()).resolve()
    return {
        "kind": "musl-smoke",
        "recipe_version": SMOKE_RECIPE_VERSION,
        "sysroot_fingerprint": sysroot_fingerprint,
        "compiler": compiler_identity(compiler),
        "source_sha256": sha256(ROOT / "user" / "musl-smoke.c"),
        "link_arguments": list(SMOKE_LINK_ARGUMENTS),
        "libgcc": {"path": str(libgcc), "sha256": sha256(libgcc)},
        "install": str(install),
    }


def link_smoke(
    install: Path,
    compiler: Path,
    sysroot_fingerprint: str,
    rebuild: bool = False,
) -> Path:
    """按 consumer/sysroot/link recipe fingerprint 链接或复用静态 ET_EXEC。"""
    payload = smoke_payload(install, compiler, sysroot_fingerprint)
    smoke_fingerprint = fingerprint(payload)
    directory = WORK / "smoke" / smoke_fingerprint
    output = directory / "musl-smoke"
    if not rebuild and manifest_matches(directory, payload, ("musl-smoke",)):
        print(f"musl smoke cache hit: {smoke_fingerprint[:12]}")
        return output

    generation = generation_directory(WORK / "smoke-generations", smoke_fingerprint)
    generation_output = generation / "musl-smoke"
    libgcc = run([str(compiler), "-print-libgcc-file-name"], ROOT).strip()
    published = False
    try:
        run(
            [
                str(compiler),
                *SMOKE_LINK_ARGUMENTS,
                f"-I{install / 'include'}",
                "-o",
                str(generation_output),
                str(install / "lib" / "crt1.o"),
                str(install / "lib" / "crti.o"),
                str(ROOT / "user" / "musl-smoke.c"),
                f"-L{install / 'lib'}",
                "-Wl,--start-group",
                "-lc",
                libgcc,
                "-Wl,--end-group",
                str(install / "lib" / "crtn.o"),
            ],
            ROOT,
        )
        write_manifest(generation, payload)
        publish_generation(generation, directory)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    print(f"musl smoke cache populated: {smoke_fingerprint[:12]}")
    return output


def verify_elf(binary: Path, compiler: Path) -> None:
    """拒绝动态/TLS/WX 产物，并证明 PHDR 位于 offset-zero LOAD。"""
    prefix = str(compiler)[: -len("gcc")]
    readelf = Path(f"{prefix}readelf")
    if not readelf.is_file():
        candidate = shutil.which("llvm-readelf") or "/opt/homebrew/opt/llvm/bin/llvm-readelf"
        readelf = Path(candidate)
    if not readelf.is_file():
        raise RuntimeError("RISC-V readelf or llvm-readelf is required")
    output = run(
        [str(readelf), "--file-header", "--program-headers", "--wide", str(binary)], ROOT
    )
    for marker in ("ELF64", "RISC-V", "EXEC"):
        if marker not in output:
            raise RuntimeError(f"musl smoke ELF lacks {marker!r}")
    headers = [line.split() for line in output.splitlines()]
    if any(columns and columns[0] in {"INTERP", "DYNAMIC", "TLS"} for columns in headers):
        raise RuntimeError("musl smoke must remain a static non-TLS ET_EXEC")
    loads = [columns for columns in headers if columns and columns[0] == "LOAD"]
    if not loads or not any(int(columns[1], 16) == 0 for columns in loads):
        raise RuntimeError("musl smoke PHDR table is not covered by an offset-zero LOAD")
    for columns in headers:
        if len(columns) < 8 or columns[0] not in {"LOAD", "GNU_STACK"}:
            continue
        flags = "".join(columns[6:-1])
        if columns[0] == "LOAD" and "W" in flags and "E" in flags:
            raise RuntimeError("musl smoke contains a writable executable LOAD")
        if columns[0] == "GNU_STACK" and "E" in flags:
            raise RuntimeError("musl smoke requests an executable stack")


def create_image(binary: Path) -> Path:
    image = WORK / "fs.img"
    run(
        [
            sys.executable,
            "create_fs.py",
            "create",
            "--file",
            str(image),
            "--init",
            str(binary),
        ],
        ROOT,
    )
    return image


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--build-only",
        action="store_true",
        help="只构建并校验固定 musl consumer，不创建镜像或启动 QEMU",
    )
    parser.add_argument(
        "--rebuild",
        action="store_true",
        help="忽略当前 fingerprint 的 sysroot/smoke 命中并重新构建",
    )
    args = parser.parse_args()
    try:
        jobs_override = build_jobs_override()
        compiler = find_compiler()
        with cache_lock(WORK / ".build.lock"):
            source = obtain_source()
            install, sysroot_fingerprint = build_musl(
                source, compiler, jobs_override, args.rebuild
            )
            binary = link_smoke(install, compiler, sysroot_fingerprint, args.rebuild)
            verify_elf(binary, compiler)
        if args.build_only:
            print(f"musl {MUSL_VERSION} static userspace build passed")
            return 0
        image = create_image(binary)
        boot(
            image,
            1,
            (
                "dynamic hart topology initialized: count=1, mask=0x1",
                "all DTB harts online: count=1, mask=0x1",
                "LiteOS musl pthread signal ok",
            ),
        )
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"musl verification failed: {error}", file=sys.stderr)
        return 1
    print(f"musl {MUSL_VERSION} pthread signal verification passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
