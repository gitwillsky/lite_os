#!/usr/bin/env python3
"""构建固定 musl pthread consumer，并通过 ELF 与 QEMU 冷启动围栏。"""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

from qemu_gate import boot

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target" / "musl-static"
MUSL_VERSION = "1.2.6"
MUSL_REVISION = "9fa28ece75d8a2191de7c5bb53bed224c5947417"
MUSL_URL = f"https://musl.libc.org/releases/musl-{MUSL_VERSION}.tar.gz"
MUSL_SHA256 = "d585fd3b613c66151fc3249e8ed44f77020cb5e6c1e635a616d3f9f82460512a"


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


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def obtain_source() -> Path:
    """获取并校验官方 release tarball，不接受同版本的其他来源。"""
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

    source = WORK / "source"
    shutil.rmtree(source, ignore_errors=True)
    source.mkdir(parents=True)
    run(
        ["tar", "-xzf", str(archive), "--strip-components=1", "-C", str(source)],
        ROOT,
    )
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


def build_musl(source: Path, compiler: Path) -> Path:
    """用当前平台唯一可用的 RISC-V GCC 构建固定静态 musl。"""
    build = WORK / "build"
    install = WORK / "install"
    shutil.rmtree(build, ignore_errors=True)
    shutil.rmtree(install, ignore_errors=True)
    build.mkdir(parents=True)
    prefix = str(compiler)[: -len("gcc")]
    env = os.environ.copy()
    env["LC_ALL"] = "C"
    for name in ("CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "LIBRARY_PATH"):
        env.pop(name, None)
    run(
        [
            str(source / "configure"),
            "--target=riscv64",
            f"--prefix={install}",
            "--disable-shared",
            f"CROSS_COMPILE={prefix}",
        ],
        build,
        env,
    )
    jobs = str(min(os.cpu_count() or 1, 8))
    run(["make", f"-j{jobs}"], build, env)
    run(["make", "install"], build, env)
    return install


def link_smoke(install: Path, compiler: Path) -> Path:
    """链接单一静态 ET_EXEC consumer，不使用宿主 libc 或默认 crt。"""
    output = WORK / "musl-smoke"
    libgcc = run([str(compiler), "-print-libgcc-file-name"], ROOT).strip()
    run(
        [
            str(compiler),
            "-static",
            "-no-pie",
            "-nostdlib",
            "-nostartfiles",
            "-ffreestanding",
            "-fno-stack-protector",
            "-march=rv64gc",
            "-mabi=lp64d",
            f"-I{install / 'include'}",
            "-Wl,--gc-sections",
            "-Wl,-Ttext-segment=0x10000",
            "-o",
            str(output),
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
    try:
        WORK.mkdir(parents=True, exist_ok=True)
        compiler = find_compiler()
        source = obtain_source()
        install = build_musl(source, compiler)
        binary = link_smoke(install, compiler)
        verify_elf(binary, compiler)
        image = create_image(binary)
        boot(
            image,
            1,
            (
                "dynamic hart topology initialized: count=1, mask=0x1",
                "all DTB harts online: count=1, mask=0x1",
                "LiteOS musl pthread ok",
            ),
        )
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"musl verification failed: {error}", file=sys.stderr)
        return 1
    print("musl 1.2.6 pthread verification passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
