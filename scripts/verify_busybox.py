#!/usr/bin/env python3
"""构建固定上游 BusyBox 静态 ET_EXEC，并校验唯一受控配置与 ELF 边界。"""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

from verify_musl import find_compiler, run

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target" / "busybox-static"
MUSL_INSTALL = ROOT / "target" / "musl-static" / "install"
MUSL_SOURCE = ROOT / "target" / "musl-static" / "source"
CONFIG_FRAGMENT = ROOT / "user" / "busybox.config"
BUSYBOX_VERSION = "1.37.0"
BUSYBOX_URL = f"https://busybox.net/downloads/busybox-{BUSYBOX_VERSION}.tar.bz2"
BUSYBOX_SHA256 = "3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def obtain_source() -> Path:
    """获取并校验官方 release tarball，不接受同版本的其他来源。"""
    archive = WORK / f"busybox-{BUSYBOX_VERSION}.tar.bz2"
    if not archive.is_file() or sha256(archive) != BUSYBOX_SHA256:
        archive.unlink(missing_ok=True)
        temporary = archive.with_suffix(".download")
        temporary.unlink(missing_ok=True)
        print(f"downloading BusyBox {BUSYBOX_VERSION}")
        try:
            urllib.request.urlretrieve(BUSYBOX_URL, temporary)
        except Exception as error:
            temporary.unlink(missing_ok=True)
            raise RuntimeError(f"failed to download {BUSYBOX_URL}: {error}") from error
        if sha256(temporary) != BUSYBOX_SHA256:
            temporary.unlink(missing_ok=True)
            raise RuntimeError("BusyBox release tarball SHA-256 mismatch")
        temporary.replace(archive)

    source = WORK / "source"
    shutil.rmtree(source, ignore_errors=True)
    source.mkdir(parents=True)
    run(
        ["tar", "-xjf", str(archive), "--strip-components=1", "-C", str(source)],
        ROOT,
    )
    return source


def fragment_assignments(path: Path) -> dict[str, str]:
    """读取显式赋值；生成配置中的其他 symbol 必须保持 allnoconfig 默认值。"""
    assignments: dict[str, str] = {}
    for raw_line in path.read_text().splitlines():
        line = raw_line.strip()
        if line.startswith("CONFIG_") and "=" in line:
            name = line.split("=", 1)[0]
        elif line.startswith("# CONFIG_") and line.endswith(" is not set"):
            name = line[2 : line.index(" is not set")]
        else:
            continue
        if name in assignments:
            raise RuntimeError(f"duplicate BusyBox config assignment: {name}")
        assignments[name] = line
    return assignments


def configure(source: Path, env: dict[str, str]) -> None:
    """从全关闭状态应用唯一 fragment，避免 BusyBox 默认 applet 隐式进入产物。"""
    run(["make", "allnoconfig"], source, env)
    config = source / ".config"
    lines = config.read_text().splitlines()
    assignments = fragment_assignments(CONFIG_FRAGMENT)
    replaced: set[str] = set()
    for index, line in enumerate(lines):
        if line.startswith("CONFIG_") and "=" in line:
            name = line.split("=", 1)[0]
        elif line.startswith("# CONFIG_") and line.endswith(" is not set"):
            name = line[2 : line.index(" is not set")]
        else:
            continue
        if name in assignments:
            lines[index] = assignments[name]
            replaced.add(name)
    missing = sorted(assignments.keys() - replaced)
    if missing:
        raise RuntimeError(f"BusyBox config contains unknown symbols: {', '.join(missing)}")
    config.write_text("\n".join(lines) + "\n")

    result = subprocess.run(
        ["make", "oldconfig"],
        cwd=source,
        env=env,
        input="\n" * 2048,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-80:])
        raise RuntimeError(f"BusyBox oldconfig failed\n{tail}")
    resolved = config.read_text().splitlines()
    resolved_set = set(resolved)
    drift = [line for line in assignments.values() if line not in resolved_set]
    if drift:
        raise RuntimeError(f"BusyBox rejected required config: {', '.join(drift)}")


def build_busybox(source: Path, compiler: Path) -> Path:
    """使用上一 gate 产出的固定 musl sysroot 构建静态 BusyBox。"""
    if not (MUSL_INSTALL / "lib" / "libc.a").is_file() or not MUSL_SOURCE.is_dir():
        raise RuntimeError("musl gate must run before BusyBox gate")
    env = os.environ.copy()
    env["LC_ALL"] = "C"
    for name in ("CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "LIBRARY_PATH"):
        env.pop(name, None)
    configure(source, env)

    specs = WORK / "musl-gcc.specs"
    result = subprocess.run(
        [
            "sh",
            str(MUSL_SOURCE / "tools" / "musl-gcc.specs.sh"),
            str(MUSL_INSTALL / "include"),
            str(MUSL_INSTALL / "lib"),
            "/lib/ld-musl-riscv64.so.1",
        ],
        cwd=ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"failed to generate musl GCC specs\n{result.stdout}")
    specs_text = result.stdout
    if run([str(compiler), "-print-file-name=crtbeginS.o"], ROOT).strip() == "crtbeginS.o":
        # bare-metal GCC 只提供等价的静态 crtbegin/crtend；缺少此适配会在最终链接时误报库探测失败。
        specs_text = specs_text.replace("crtbeginS.o%s", "crtbegin.o%s")
        specs_text = specs_text.replace("crtendS.o%s", "crtend.o%s")
        # 同一工具链默认追加 newlib 的 libgloss；musl 静态链接必须只有唯一 libc provider。
        specs_text = specs_text.replace(
            "%rename cpp_options old_cpp_options",
            "%rename cpp_options old_cpp_options\n%rename lib old_lib",
            1,
        )
        specs_text = specs_text.replace("\n*esp_link:", "\n*lib:\n-lc\n\n*esp_link:", 1)
    specs.write_text(specs_text)

    prefix = str(compiler)[: -len("gcc")]
    jobs = str(min(os.cpu_count() or 1, 8))
    run(
        [
            "make",
            f"-j{jobs}",
            "ARCH=riscv",
            f"CROSS_COMPILE={prefix}",
            f"CC={compiler} -specs={specs}",
        ],
        source,
        env,
    )
    binary = source / "busybox"
    if not binary.is_file():
        raise RuntimeError("BusyBox build did not produce busybox")
    return binary


def verify_elf(binary: Path, compiler: Path) -> None:
    """要求静态 RISC-V ET_EXEC、非 W+X LOAD 与不可执行用户栈。"""
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
            raise RuntimeError(f"BusyBox ELF lacks {marker!r}")
    headers = [line.split() for line in output.splitlines()]
    if any(columns and columns[0] in {"INTERP", "DYNAMIC"} for columns in headers):
        raise RuntimeError("BusyBox must remain a static ET_EXEC")
    loads = [columns for columns in headers if columns and columns[0] == "LOAD"]
    if not loads or not any(int(columns[1], 16) == 0 for columns in loads):
        raise RuntimeError("BusyBox PHDR table is not covered by an offset-zero LOAD")
    for columns in headers:
        if len(columns) < 8 or columns[0] not in {"LOAD", "GNU_STACK"}:
            continue
        flags = "".join(columns[6:-1])
        if columns[0] == "LOAD" and "W" in flags and "E" in flags:
            raise RuntimeError("BusyBox contains a writable executable LOAD")
        if columns[0] == "GNU_STACK" and "E" in flags:
            raise RuntimeError("BusyBox requests an executable stack")


def main() -> int:
    try:
        WORK.mkdir(parents=True, exist_ok=True)
        compiler = find_compiler()
        source = obtain_source()
        binary = build_busybox(source, compiler)
        verify_elf(binary, compiler)
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"BusyBox verification failed: {error}", file=sys.stderr)
        return 1
    print(f"BusyBox {BUSYBOX_VERSION} static build verification passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
