#!/usr/bin/env python3
"""构建固定 musl pthread consumer，并通过 ELF 与 QEMU 冷启动围栏。"""

from __future__ import annotations

import argparse
import os
import shutil
import struct
import tempfile
import subprocess
import sys
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from local_inputs import INPUTS

from build_target import target_from_environment

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
    publish_runtime_gate,
    runtime_gate_hit,
    runtime_gate_payload,
    sha256,
    temporary_directory,
    write_manifest,
)

from qemu_gate import boot, cpu_topology_markers

ROOT = Path(__file__).resolve().parent.parent
TARGET = target_from_environment()
WORK = ROOT / "target" / "musl-runtime" / TARGET.arch
SOURCE = INPUTS / "musl" / TARGET.arch
MUSL_VERSION = "1.2.6"
MUSL_REVISION = "9fa28ece75d8a2191de7c5bb53bed224c5947417"
MUSL_URLS = (
    f"https://mirrors.aliyun.com/gentoo/distfiles/9d/musl-{MUSL_VERSION}.tar.gz",
    f"https://mirrors.tuna.tsinghua.edu.cn/gentoo/distfiles/9d/musl-{MUSL_VERSION}.tar.gz",
)
MUSL_SHA256 = "d585fd3b613c66151fc3249e8ed44f77020cb5e6c1e635a616d3f9f82460512a"
LINUX_VERSION = "7.1"
# 国内 kernel.org 镜像优先，官方 CDN 兜底；全部按 LINUX_SHA256 裁决。缺少回退时官方 CDN 的
# 瞬时 TLS 故障会让首次构建该架构 sysroot 的 gate 直接失败。
LINUX_URLS = (
    f"https://mirrors.tuna.tsinghua.edu.cn/kernel/v7.x/linux-{LINUX_VERSION}.tar.xz",
    f"https://mirrors.aliyun.com/linux-kernel/v7.x/linux-{LINUX_VERSION}.tar.xz",
    f"https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-{LINUX_VERSION}.tar.xz",
)
LINUX_SHA256 = "691f44797fbe790dc8a321604c927087526ad27b6d649925d60f8eed0a2564a0"
MAKE_VERSION = "4.4.1"
MAKE_URL = f"https://mirrors.kernel.org/gnu/make/make-{MAKE_VERSION}.tar.lz"
MAKE_SHA256 = "8814ba072182b605d156d7589c19a43b89fc58ea479b9355146160946f8cf6e9"
# compiler-builtins 自 nightly-2026-10-03 起只以 `extern "Rust"` 导出 f128 complex helper
# （`__rust_multc3`），不再提供 C ABI `__multc3`；Clang 把 `long double _Complex` 乘法 lower
# 为该符号，缺少补充时 musl `libc.so` 在 `cpowl.lo` 处以 undefined `__multc3` 链接失败。
# 补充源码固定为 rustc 报告的同一 LLVM release；逐文件 SHA-256 覆盖 multc3.c 的完整 include 闭包。
COMPILER_RT_VERSION = "23.1.1"
# jsDelivr GitHub CDN 优先，raw.githubusercontent.com 兜底；两者都按下方逐文件摘要校验。
COMPILER_RT_URLS = (
    f"https://cdn.jsdelivr.net/gh/llvm/llvm-project@llvmorg-{COMPILER_RT_VERSION}"
    "/compiler-rt/lib/builtins/",
    f"https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-{COMPILER_RT_VERSION}"
    "/compiler-rt/lib/builtins/",
)
COMPILER_RT_SOURCES = {
    "multc3.c": "826cabd0ba1ac5a249b1ff5661b26a98aba7ebcbc251fdaebed64c20216e968c",
    "fp_lib.h": "cf3a5301c95986a9b392edba32dfe7ac27d3352d266e284e528a09413f84f6cc",
    "int_lib.h": "a5735bf1bfacecd6cf5785ed19ae62a981ff2f0923c38caa99f4af47f2be57b5",
    "int_math.h": "ddc3abe104d4500630174d7e0c3190d203c968213eb189097f70aec55a33c352",
    "int_types.h": "198552b25faf8a75b077ea145591cd7f6762b7f2712321b18f5e38fee8cc2a1e",
    "int_util.h": "63d06198ab720a75c20bfd84be895c2746581b129168bae6201e7384bbb0473b",
    "int_endianness.h": "ec5175561075845b7b16a92ad5ecdfd42a284de2736dd07e4ca7011b75c37709",
}
COMPILER_RT_SUPPLEMENT_SYMBOL = "__multc3"
SOURCE_RECIPE_VERSION = 1
SYSROOT_RECIPE_VERSION = 4
SMOKE_RECIPE_VERSION = 21
CONFIGURE_ARGUMENTS = (f"--target={TARGET.arch}", "--prefix=/usr", "--syslibdir=/lib")
COMMON_SMOKE_LINK_ARGUMENTS = (
    "-static",
    "-nostdlib",
    "-nostartfiles",
    "-ffreestanding",
    "-fno-stack-protector",
    "-Wl,--gc-sections",
)
if TARGET.arch == "aarch64":
    SMOKE_LINK_ARGUMENTS = (
        *COMMON_SMOKE_LINK_ARGUMENTS,
        "-Wl,--image-base=0x10000",
        "-march=armv8-a",
    )
    LINUX_HEADER_ARCH = "arm64"
    ELF_MACHINE = "AArch64"
    # hard-float AAPCS64；kernel 的 softfloat builtins 链接进 musl 会让 FP helper return ABI 分裂。
    RUST_BUILTINS_TARGET = "aarch64-unknown-none"
    COMPILER_RT_TARGET_FLAGS = ("-march=armv8-a",)
else:
    SMOKE_LINK_ARGUMENTS = (
        *COMMON_SMOKE_LINK_ARGUMENTS,
        "-no-pie",
        "-Wl,--image-base=0x10000",
        "-march=rv64gc",
        "-mabi=lp64d",
    )
    LINUX_HEADER_ARCH = "riscv"
    ELF_MACHINE = "RISC-V"
    # lp64d medany：代码只含 PC-relative 重定位，可进入 libc.so。Homebrew 只提供裸机
    # riscv64-elf libgcc（medlow 绝对 HI20/LO12），链接 shared libc 会失败。
    RUST_BUILTINS_TARGET = "riscv64gc-unknown-none-elf"
    COMPILER_RT_TARGET_FLAGS = ("-march=rv64gc", "-mabi=lp64d")


@dataclass(frozen=True)
class MuslCachePaths:
    source: Path
    install: Path
    sysroot_fingerprint: str
    compiler: Path
    linker: Path
    archiver: Path
    ranlib: Path
    compiler_runtime: Path


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
    return SOURCE / "sources" / fingerprint(source_payload())


def obtain_source() -> Path:
    """获取并缓存固定源码；完整目录只在校验和解压成功后发布。"""
    SOURCE.mkdir(parents=True, exist_ok=True)
    archive = SOURCE / f"musl-{MUSL_VERSION}.tar.gz"
    if not archive.is_file() or sha256(archive) != MUSL_SHA256:
        archive.unlink(missing_ok=True)
        temporary = archive.with_suffix(".download")
        temporary.unlink(missing_ok=True)
        errors = []
        for url in MUSL_URLS:
            print(f"downloading musl {MUSL_VERSION} ({MUSL_REVISION[:12]}) from {url}")
            try:
                urllib.request.urlretrieve(url, temporary)
            except Exception as error:
                errors.append(f"{url}: {error}")
                temporary.unlink(missing_ok=True)
                continue
            if sha256(temporary) != MUSL_SHA256:
                errors.append(f"{url}: SHA-256 mismatch")
                temporary.unlink(missing_ok=True)
                continue
            break
        else:
            temporary.unlink(missing_ok=True)
            raise RuntimeError(
                "failed to download musl release tarball:\n" + "\n".join(errors)
            )
        temporary.replace(archive)

    payload = source_payload()
    source = source_cache_path()
    if manifest_matches(source, payload, ("configure", "tools/musl-gcc.specs.sh")):
        return source

    temporary = temporary_directory(SOURCE / "sources", "source")
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


def obtain_linux_source() -> Path:
    """获取固定 Linux 基线，作为 sysroot 中 UAPI headers 的唯一来源。"""
    SOURCE.mkdir(parents=True, exist_ok=True)
    payload = {
        "kind": "linux-uapi-source",
        "version": LINUX_VERSION,
        "archive_sha256": LINUX_SHA256,
        "strip_components": 1,
    }
    archive = SOURCE / f"linux-{LINUX_VERSION}.tar.xz"
    if not archive.is_file() or sha256(archive) != LINUX_SHA256:
        archive.unlink(missing_ok=True)
        temporary_archive = archive.with_suffix(".download")
        temporary_archive.unlink(missing_ok=True)
        errors = []
        for url in LINUX_URLS:
            print(f"downloading Linux UAPI {LINUX_VERSION} from {url}")
            try:
                urllib.request.urlretrieve(url, temporary_archive)
            except Exception as error:
                errors.append(f"{url}: {error}")
                temporary_archive.unlink(missing_ok=True)
                continue
            if sha256(temporary_archive) != LINUX_SHA256:
                errors.append(f"{url}: SHA-256 mismatch")
                temporary_archive.unlink(missing_ok=True)
                continue
            break
        else:
            raise RuntimeError("failed to download Linux release tarball:\n" + "\n".join(errors))
        temporary_archive.replace(archive)
    source = SOURCE / "linux-sources" / fingerprint(payload)
    if manifest_matches(source, payload, ("Makefile", "include/uapi/linux/vt.h")):
        return source
    temporary = temporary_directory(SOURCE / "linux-sources", "source")
    try:
        run(["tar", "-xJf", str(archive), "--strip-components=1", "-C", str(temporary)], ROOT)
        write_manifest(temporary, payload)
        publish_directory(temporary, source)
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    return source


def obtain_gnu_make(jobs_override: int | None) -> Path:
    """构建仓库私有 GNU Make，避免 Linux UAPI 生成依赖 host make 版本。"""
    SOURCE.mkdir(parents=True, exist_ok=True)
    payload = {"kind": "gnu-make", "version": MAKE_VERSION, "archive_sha256": MAKE_SHA256}
    install = WORK / "tools" / fingerprint(payload)
    if manifest_matches(install, payload, ("bin/make",)):
        return install / "bin/make"
    archive = SOURCE / f"make-{MAKE_VERSION}.tar.lz"
    if not archive.is_file() or sha256(archive) != MAKE_SHA256:
        archive.unlink(missing_ok=True)
        temporary_archive = archive.with_suffix(".download")
        urllib.request.urlretrieve(MAKE_URL, temporary_archive)
        if sha256(temporary_archive) != MAKE_SHA256:
            temporary_archive.unlink(missing_ok=True)
            raise RuntimeError("GNU Make release tarball SHA-256 mismatch")
        temporary_archive.replace(archive)
    build = temporary_directory(WORK / "tool-builds", "make")
    generation = generation_directory(WORK / "tool-generations", fingerprint(payload))
    published = False
    try:
        run(["tar", "-xf", str(archive), "--strip-components=1", "-C", str(build)], ROOT)
        run([str(build / "configure"), f"--prefix={generation}", "--disable-nls"], build)
        run(make_command(jobs_override), build)
        run(["make", "install"], build)
        write_manifest(generation, payload)
        publish_generation(generation, install)
        published = True
    finally:
        shutil.rmtree(build, ignore_errors=True)
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    return install / "bin/make"


def find_compiler() -> Path:
    """返回两个架构共用的 userspace compiler driver（Clang，按 ``--target`` 选择后端）。"""
    return find_clang()


def find_clang() -> Path:
    """定位包含当前 guest backend 的 Clang compiler driver。"""
    for candidate in (
        shutil.which("clang"),
        "/opt/homebrew/opt/llvm/bin/clang",
        "/usr/local/opt/llvm/bin/clang",
    ):
        if candidate is None or not Path(candidate).is_file():
            continue
        targets = subprocess.run(
            [candidate, "--print-targets"],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        if targets.returncode == 0 and any(
            line.lstrip().startswith(f"{TARGET.arch} ")
            for line in targets.stdout.splitlines()
        ):
            return Path(candidate).resolve()
    raise RuntimeError(f"Clang with the {TARGET.arch} backend is required")


def find_stripper(compiler: Path) -> Path:
    """选择所有 Clang userspace consumer 共用的 LLVM strip，保留 argv[0] basename。"""
    for candidate in (
        shutil.which("llvm-strip"),
        compiler.resolve().with_name("llvm-strip"),
        "/opt/homebrew/opt/llvm/bin/llvm-strip",
        "/usr/local/opt/llvm/bin/llvm-strip",
    ):
        if candidate is not None:
            path = Path(candidate)
            if path.is_file() and os.access(path, os.X_OK):
                # LLVM 的 llvm-strip 可是 llvm-objcopy 的 symlink；argv[0] 决定 strip 模式。
                # canonicalization 会把 strip 参数交给 objcopy 并使构建失败。
                return path.absolute()
    raise RuntimeError("LLVM llvm-strip is required to build userspace")


def rustc_probe_environment() -> dict[str, str]:
    """返回不携带失效 GNU Make jobserver capability 的 rustc 探测环境。

    Python recipe 不继承 GNU Make 3.81 的 jobserver 文件描述符，但环境仍可能保留
    ``MAKEFLAGS``/``MFLAGS``。若继续传给 rustc，bad-fd warning 会和被捕获的 stdout 合并，
    从而把单行 sysroot 误解析成不存在的多行路径。
    """
    environment = os.environ.copy()
    environment.pop("MAKEFLAGS", None)
    environment.pop("MFLAGS", None)
    return environment


def compile_static_guest_program(
    compiler: Path,
    musl: "MuslCachePaths",
    sources: list[Path],
    output: Path,
) -> None:
    """用与 musl smoke 相同的 freestanding 静态链接配方编译 guest 程序。

    Args:
        compiler: ``find_compiler`` 返回的 Clang driver。
        musl: 已构建的 musl sysroot。
        sources: 按链接顺序给出的 C 源文件。
        output: 输出 ELF 路径。

    Raises:
        RuntimeError: 编译或链接失败。
    """
    install = musl.install
    run(
        [
            str(compiler),
            f"--target={TARGET.linux_triple}",
            f"--ld-path={install / 'toolchain/ld.lld'}",
            *SMOKE_LINK_ARGUMENTS,
            f"-I{install / 'usr/include'}",
            "-O1",
            "-o",
            str(output),
            str(install / "usr/lib" / "crt1.o"),
            str(install / "usr/lib" / "crti.o"),
            *(str(source) for source in sources),
            f"-L{install / 'usr/lib'}",
            "-Wl,--start-group",
            str(install / "usr/lib/libc.a"),
            str(obtain_compiler_runtime(compiler)),
            "-Wl,--end-group",
            str(install / "usr/lib" / "crtn.o"),
        ],
        ROOT,
    )


def obtain_compiler_runtime(compiler: Path) -> Path:
    """返回目标 compiler runtime archive。

    两个架构都使用固定 Rust toolchain 的 bare-metal ``compiler_builtins``，并按
    ``COMPILER_RT_SOURCES`` 补入 C ABI ``__multc3``，合并为单一 archive，使所有 consumer 仍只
    消费一条 runtime path。

    Args:
        compiler: 已定位的 userspace compiler driver。

    Returns:
        已发布且内容身份匹配的 runtime archive 绝对路径。

    Raises:
        RuntimeError: runtime 缺失、补充源码摘要不匹配，或固定 toolchain 已自行导出补充符号。
    """
    builtins = pinned_compiler_builtins()
    clang, _, archiver, ranlib = find_runtime_toolchain()
    sources = obtain_compiler_rt_sources()
    flags = (
        f"--target={TARGET.linux_triple}",
        *COMPILER_RT_TARGET_FLAGS,
        "-std=c11",
        # 单个 complex 冷路径 helper，固定 O2 与上游 builtins 一致；正确性由 musl smoke 的
        # Annex G 重算用例裁决，不增加 host benchmark。
        "-O2",
        "-fPIC",
        "-ffreestanding",
        # 只消费 compiler builtin headers；host 的 Linux/glibc headers 不能进入 target runtime。
        # Debian Clang 的 musl include 顺序先查 host /usr/include，仅 freestanding 不能阻止泄漏。
        "-nostdlibinc",
        "-fno-builtin",
        "-ffunction-sections",
        "-fdata-sections",
        "-fvisibility=hidden",
    )
    payload = {
        "kind": f"{TARGET.arch}-compiler-runtime",
        "recipe_version": 2,
        "compiler_builtins_sha256": sha256(builtins),
        "compiler_rt": {"version": COMPILER_RT_VERSION, "sha256": COMPILER_RT_SOURCES},
        "clang": tool_identity(clang),
        "archiver": tool_identity(archiver),
        "flags": list(flags),
    }
    identity = fingerprint(payload)
    entry = WORK / "compiler-runtime" / identity
    if manifest_matches(entry, payload, ("libcompiler-runtime.a",)):
        return (entry / "libcompiler-runtime.a").resolve()

    # 补充只覆盖 toolchain 缺口；toolchain 恢复导出后必须删除补充，避免两份实现并存。
    defined = run(
        [str(archiver.with_name("llvm-nm")), "--defined-only", "--format=just-symbols", str(builtins)],
        ROOT,
    ).split()
    if COMPILER_RT_SUPPLEMENT_SYMBOL in defined:
        raise RuntimeError(
            f"pinned compiler_builtins already exports {COMPILER_RT_SUPPLEMENT_SYMBOL}; "
            "remove the compiler-rt supplement from verify_musl.py"
        )
    generation = generation_directory(WORK / "compiler-runtime-generations", identity)
    try:
        supplement = generation / "multc3.o"
        run([str(clang), *flags, "-I", str(sources), "-c", str(sources / "multc3.c"), "-o", str(supplement)], ROOT)
        archive = generation / "libcompiler-runtime.a"
        shutil.copyfile(builtins, archive)
        run([str(archiver), "q", str(archive), str(supplement)], ROOT)
        run([str(ranlib), str(archive)], ROOT)
        supplement.unlink()
        write_manifest(generation, payload)
        publish_generation(generation, entry)
    except BaseException:
        shutil.rmtree(generation, ignore_errors=True)
        raise
    return (entry / "libcompiler-runtime.a").resolve()


def pinned_compiler_builtins() -> Path:
    """定位固定 Rust toolchain 中 ``RUST_BUILTINS_TARGET`` 的 ``compiler_builtins`` rlib。"""
    rust_sysroot = Path(
        run(
            ["rustc", "--print", "sysroot"],
            ROOT,
            env=rustc_probe_environment(),
        ).strip()
    )
    candidates = tuple(
        sorted(
            (
                rust_sysroot
                / f"lib/rustlib/{RUST_BUILTINS_TARGET}/lib"
            ).glob("libcompiler_builtins-*.rlib")
        )
    )
    if len(candidates) != 1 or not candidates[0].is_file():
        raise RuntimeError(
            f"pinned Rust {RUST_BUILTINS_TARGET} compiler_builtins runtime "
            f"is missing or ambiguous under {rust_sysroot}"
        )
    return candidates[0].resolve()


def obtain_compiler_rt_sources() -> Path:
    """下载并缓存固定 compiler-rt builtins 补充源码；全部摘要匹配后才发布目录。"""
    payload = {
        "kind": "compiler-rt-builtins-source",
        "version": COMPILER_RT_VERSION,
        "sha256": COMPILER_RT_SOURCES,
    }
    source = SOURCE / "compiler-rt-sources" / fingerprint(payload)
    if manifest_matches(source, payload, tuple(COMPILER_RT_SOURCES)):
        return source
    temporary = temporary_directory(SOURCE / "compiler-rt-sources", "source")
    try:
        print(f"downloading compiler-rt {COMPILER_RT_VERSION} builtins supplement")
        for name, digest in COMPILER_RT_SOURCES.items():
            path = temporary / name
            errors = []
            for base in COMPILER_RT_URLS:
                try:
                    urllib.request.urlretrieve(base + name, path)
                except Exception as error:
                    errors.append(f"{base}{name}: {error}")
                    continue
                if sha256(path) == digest:
                    break
                errors.append(f"{base}{name}: SHA-256 mismatch")
            else:
                raise RuntimeError(
                    f"failed to download compiler-rt {COMPILER_RT_VERSION} {name}:\n" + "\n".join(errors)
                )
        write_manifest(temporary, payload)
        publish_directory(temporary, source)
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    return source


def find_runtime_toolchain() -> tuple[Path, Path, Path, Path]:
    """定位能产生 Linux ET_DYN 的 Clang/LLD，以及配套 LLVM archive 工具。

    Returns:
        Clang、pinned rust-lld、llvm-ar 与保留 argv[0] 的 llvm-ranlib 路径。

    Raises:
        RuntimeError: 任一固定工具不存在或 rustc 无法报告 sysroot。
    """
    clang = find_clang()
    archiver = shutil.which("llvm-ar") or "/opt/homebrew/opt/llvm/bin/llvm-ar"
    ranlib = shutil.which("llvm-ranlib") or "/opt/homebrew/opt/llvm/bin/llvm-ranlib"
    rust_sysroot = Path(
        run(
            ["rustc", "--print", "sysroot"],
            ROOT,
            env=rustc_probe_environment(),
        ).strip()
    )
    linkers = sorted(rust_sysroot.glob("lib/rustlib/*/bin/rust-lld"))
    tools = (clang, Path(archiver), Path(ranlib))
    if not all(tool.is_file() for tool in tools) or not linkers:
        raise RuntimeError("Clang, LLVM archive tools, and the pinned Rust rust-lld are required")
    # llvm-ranlib may be a symlink to llvm-ar; resolving it changes argv[0] and makes LLVM parse
    # the archive pathname as an ar operation string instead of selecting ranlib mode.
    return clang, linkers[0].resolve(), tools[1].resolve(), tools[2].absolute()


def compiler_identity(compiler: Path) -> dict[str, object]:
    return {
        "path": str(compiler),
        "target": run([str(compiler), f"--target={TARGET.linux_triple}", "-dumpmachine"], ROOT).strip(),
        "version": run([str(compiler), "--version"], ROOT).splitlines()[0],
    }


def tool_identity(tool: Path, version_argument: str = "--version") -> dict[str, str]:
    return {"path": str(tool), "version": run([str(tool), version_argument], ROOT).splitlines()[0]}


def sysroot_payload(compiler: Path) -> dict[str, object]:
    clang, linker, archiver, ranlib = find_runtime_toolchain()
    compiler_runtime = obtain_compiler_runtime(compiler)
    return {
        "kind": "musl-runtime-sysroot",
        "recipe_version": SYSROOT_RECIPE_VERSION,
        "arch": TARGET.arch,
        "linux_target": TARGET.linux_triple,
        "source_fingerprint": fingerprint(source_payload()),
        "linux_uapi": {"version": LINUX_VERSION, "archive_sha256": LINUX_SHA256},
        "compiler": compiler_identity(compiler),
        "compiler_runtime": {
            "kind": "pinned-rust-compiler-builtins+compiler-rt-multc3",
            "path": str(compiler_runtime),
            "sha256": sha256(compiler_runtime),
        },
        "toolchain": {
            "clang": tool_identity(clang),
            "linker": {"path": str(linker), "sha256": sha256(linker)},
            "archiver": tool_identity(archiver),
            "ranlib": tool_identity(ranlib),
        },
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
    linux_source: Path,
    compiler: Path,
    jobs_override: int | None,
    rebuild: bool = False,
) -> tuple[Path, str]:
    """按 toolchain/recipe fingerprint 构建或复用唯一 musl runtime。"""
    payload = sysroot_payload(compiler)
    sysroot_fingerprint = fingerprint(payload)
    install = sysroot_cache_path(payload)
    required = ("usr/include/linux/vt.h", "usr/lib/libc.a", "usr/lib/libc.so", "usr/lib/crt1.o", "usr/lib/Scrt1.o", "usr/lib/crti.o", "usr/lib/crtn.o", "toolchain/ld.lld")
    loader = install / TARGET.musl_loader.removeprefix("/")
    if not rebuild and manifest_matches(install, payload, required) and loader.is_symlink() and os.readlink(loader) == "/usr/lib/libc.so":
        print(f"musl sysroot cache hit: {sysroot_fingerprint[:12]}")
        return install, sysroot_fingerprint

    build = temporary_directory(WORK / "builds", "build")
    generation = generation_directory(WORK / "install-generations", sysroot_fingerprint)
    clang, linker, archiver, ranlib = find_runtime_toolchain()
    compiler_runtime = obtain_compiler_runtime(compiler)
    gmake = obtain_gnu_make(jobs_override)
    lld_alias = build / "ld.lld"
    lld_alias.symlink_to(linker)
    env = build_environment()
    env.update({
        "CC": f"{clang} --target={TARGET.linux_triple} --ld-path={lld_alias}",
        "AR": str(archiver),
        "RANLIB": str(ranlib),
        "LIBCC": str(compiler_runtime),
    })
    published = False
    try:
        run(
            [
                str(source / "configure"),
                *CONFIGURE_ARGUMENTS,
            ],
            build,
            env,
        )
        run(make_command(jobs_override), build, env)
        run(
            [
                str(gmake),
                *(make_command(jobs_override)[1:]),
                f"ARCH={LINUX_HEADER_ARCH}",
                f"INSTALL_HDR_PATH={generation / 'usr'}",
                "headers_install",
            ],
            linux_source,
            env,
        )
        run(["make", f"DESTDIR={generation}", "install"], build, env)
        (generation / "toolchain").mkdir()
        (generation / "toolchain/ld.lld").symlink_to(linker)
        generation_loader = generation / TARGET.musl_loader.removeprefix("/")
        if not all((generation / relative).is_file() for relative in required) or not generation_loader.is_symlink() or os.readlink(generation_loader) != "/usr/lib/libc.so":
            raise RuntimeError("musl install is missing required runtime artifacts")
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
    required = ("usr/include/linux/vt.h", "usr/lib/libc.a", "usr/lib/libc.so", "usr/lib/crt1.o", "usr/lib/Scrt1.o", "usr/lib/crti.o", "usr/lib/crtn.o", "toolchain/ld.lld")
    loader = install / TARGET.musl_loader.removeprefix("/")
    if not manifest_matches(install, payload, required) or not loader.is_symlink() or os.readlink(loader) != "/usr/lib/libc.so":
        raise RuntimeError("musl sysroot cache is missing; run verify_musl.py first")
    clang, linker, archiver, ranlib = find_runtime_toolchain()
    compiler_runtime = obtain_compiler_runtime(compiler)
    resolved_install = install.resolve()
    return MuslCachePaths(
        source.resolve(),
        resolved_install,
        fingerprint(payload),
        clang,
        resolved_install / "toolchain/ld.lld",
        archiver,
        ranlib,
        compiler_runtime,
    )


def smoke_payload(install: Path, compiler: Path, sysroot_fingerprint: str) -> dict[str, object]:
    compiler_runtime = obtain_compiler_runtime(compiler)
    return {
        "kind": "musl-smoke",
        "recipe_version": SMOKE_RECIPE_VERSION,
        "arch": TARGET.arch,
        "sysroot_fingerprint": sysroot_fingerprint,
        "compiler": compiler_identity(compiler),
        "source_sha256": sha256(ROOT / "scripts/fixtures/musl/musl-smoke.c"),
        "shared_sync_sha256": sha256(ROOT / "scripts/fixtures/musl/shared-sync.c"),
        "process_security_sha256": sha256(ROOT / "scripts/fixtures/musl/process-security.c"),
        "fs_context_sha256": sha256(ROOT / "scripts/fixtures/musl/fs-context.c"),
        "file_table_sha256": sha256(ROOT / "scripts/fixtures/musl/file-table.c"),
        "thread_mm_sha256": sha256(ROOT / "scripts/fixtures/musl/thread-mm.c"),
        "capabilities_sha256": sha256(ROOT / "scripts/fixtures/musl/capabilities.c"),
        "anonymous_files_sha256": sha256(ROOT / "scripts/fixtures/musl/anonymous-files.c"),
        "fs_credentials_sha256": sha256(ROOT / "scripts/fixtures/musl/fs-credentials.c"),
        "fs_root_sha256": sha256(ROOT / "scripts/fixtures/musl/fs-root.c"),
        "fs_root_dynamic_sha256": sha256(ROOT / "scripts/fixtures/musl/fs-root-dynamic.c"),
        "sighand_sha256": sha256(ROOT / "scripts/fixtures/musl/sighand.c"),
        "namespace_user_sha256": sha256(ROOT / "scripts/fixtures/musl/namespace-user.c"),
        "pidfd_sha256": sha256(ROOT / "scripts/fixtures/musl/pidfd.c"),
        "udp_sha256": sha256(ROOT / "scripts/fixtures/musl/udp.c"),
        "network_namespace_sha256": sha256(ROOT / "scripts/fixtures/musl/network-namespace.c"),
        "netlink_sha256": sha256(ROOT / "scripts/fixtures/musl/netlink.c"),
        "ipv4_routing_sha256": sha256(ROOT / "scripts/fixtures/musl/ipv4-routing.c"),
        "thread_exec_sha256": sha256(ROOT / "scripts/fixtures/musl/thread-exec.c"),
        "seccomp_sha256": sha256(ROOT / "scripts/fixtures/musl/seccomp.c"),
        "seccomp_notification_sha256": sha256(ROOT / "scripts/fixtures/musl/seccomp_notification.c"),
        "signal_frame_sha256": sha256(ROOT / "scripts/fixtures/musl/signal-frame.c"),
        "swap_sha256": sha256(ROOT / "scripts/fixtures/musl/swap.c"),
        "address_space_sha256": sha256(ROOT / "scripts/fixtures/musl/address-space.c"),
        "netlink_route_sha256": sha256(ROOT / "scripts/fixtures/musl/netlink-route.c"),
        "copy_file_range_sha256": sha256(ROOT / "scripts/fixtures/musl/copy-file-range.c"),
        "io_uring_sha256": sha256(ROOT / "scripts/fixtures/musl/io-uring.c"),
        "statx_sha256": sha256(ROOT / "scripts/fixtures/musl/statx.c"),
        "mremap_sha256": sha256(ROOT / "scripts/fixtures/musl/mremap.c"),
        "resource_usage_sha256": sha256(ROOT / "scripts/fixtures/musl/resource-usage.c"),
        "memory_pressure_sha256": sha256(ROOT / "scripts/fixtures/musl/memory-pressure.c"),
        "link_arguments": list(SMOKE_LINK_ARGUMENTS),
        "compiler_runtime": {
            "path": str(compiler_runtime),
            "sha256": sha256(compiler_runtime),
        },
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
    if not rebuild and manifest_matches(directory, payload, ("musl-smoke", "fs-root-dynamic", "libc.so")):
        print(f"musl smoke cache hit: {smoke_fingerprint[:12]}")
        return output

    generation = generation_directory(WORK / "smoke-generations", smoke_fingerprint)
    generation_output = generation / "musl-smoke"
    compiler_runtime = obtain_compiler_runtime(compiler)
    compiler_arguments = [
        str(compiler),
        f"--target={TARGET.linux_triple}",
        f"--ld-path={install / 'toolchain/ld.lld'}",
    ]
    published = False
    try:
        run(
            [
                *compiler_arguments,
                *SMOKE_LINK_ARGUMENTS,
                f"-I{install / 'usr/include'}",
                "-o",
                str(generation_output),
                str(install / "usr/lib" / "crt1.o"),
                str(install / "usr/lib" / "crti.o"),
                str(ROOT / "scripts/fixtures/musl/musl-smoke.c"),
                str(ROOT / "scripts/fixtures/musl/shared-sync.c"),
                str(ROOT / "scripts/fixtures/musl/process-security.c"),
                str(ROOT / "scripts/fixtures/musl/fs-context.c"),
                str(ROOT / "scripts/fixtures/musl/file-table.c"),
                str(ROOT / "scripts/fixtures/musl/thread-mm.c"),
                str(ROOT / "scripts/fixtures/musl/capabilities.c"),
                str(ROOT / "scripts/fixtures/musl/sighand.c"),
                str(ROOT / "scripts/fixtures/musl/fs-root.c"),
                str(ROOT / "scripts/fixtures/musl/fs-credentials.c"),
                str(ROOT / "scripts/fixtures/musl/anonymous-files.c"),
                str(ROOT / "scripts/fixtures/musl/thread-exec.c"),
                str(ROOT / "scripts/fixtures/musl/namespace-user.c"),
                str(ROOT / "scripts/fixtures/musl/pidfd.c"),
                str(ROOT / "scripts/fixtures/musl/udp.c"),
                str(ROOT / "scripts/fixtures/musl/network-namespace.c"),
                str(ROOT / "scripts/fixtures/musl/netlink.c"),
                str(ROOT / "scripts/fixtures/musl/ipv4-routing.c"),
                str(ROOT / "scripts/fixtures/musl/seccomp.c"),
                str(ROOT / "scripts/fixtures/musl/seccomp_notification.c"),
                str(ROOT / "scripts/fixtures/musl/signal-frame.c"),
                str(ROOT / "scripts/fixtures/musl/address-space.c"),
                str(ROOT / "scripts/fixtures/musl/memory-pressure.c"),
                str(ROOT / "scripts/fixtures/musl/resource-usage.c"),
                str(ROOT / "scripts/fixtures/musl/mremap.c"),
                str(ROOT / "scripts/fixtures/musl/statx.c"),
                str(ROOT / "scripts/fixtures/musl/copy-file-range.c"),
                str(ROOT / "scripts/fixtures/musl/io-uring.c"),
                str(ROOT / "scripts/fixtures/musl/netlink-route.c"),
                str(ROOT / "scripts/fixtures/musl/swap.c"),
                f"-L{install / 'usr/lib'}",
                "-Wl,--start-group",
                str(install / "usr/lib/libc.a"),
                str(compiler_runtime),
                "-Wl,--end-group",
                str(install / "usr/lib" / "crtn.o"),
            ],
            ROOT,
        )
        dynamic_arguments = [argument for argument in SMOKE_LINK_ARGUMENTS if argument != "-static"]
        run([
            *compiler_arguments, *dynamic_arguments, "-no-pie",
            f"-I{install / 'usr/include'}", f"-Wl,-dynamic-linker,{TARGET.musl_loader}",
            "-o", str(generation / "fs-root-dynamic"),
            str(install / "usr/lib/crt1.o"), str(install / "usr/lib/crti.o"),
            str(ROOT / "scripts/fixtures/musl/fs-root-dynamic.c"),
            f"-L{install / 'usr/lib'}", "-Wl,-Bdynamic", "-lc", str(install / "usr/lib/crtn.o"),
        ], ROOT)
        shutil.copy2(install / "usr/lib/libc.so", generation / "libc.so")
        write_manifest(generation, payload)
        publish_generation(generation, directory)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    print(f"musl smoke cache populated: {smoke_fingerprint[:12]}")
    return output


def verify_elf(binary: Path, compiler: Path) -> None:
    """校验静态主程序，以及同一 generation 内真实 PT_INTERP helper 的依赖。"""
    prefix = str(compiler)[: -len("gcc")]
    readelf = Path(f"{prefix}readelf")
    if not readelf.is_file():
        candidate = shutil.which("llvm-readelf") or "/opt/homebrew/opt/llvm/bin/llvm-readelf"
        readelf = Path(candidate)
    if not readelf.is_file():
        raise RuntimeError(f"{TARGET.arch} readelf or llvm-readelf is required")
    output = run(
        [str(readelf), "--file-header", "--program-headers", "--wide", str(binary)], ROOT
    )
    for marker in ("ELF64", ELF_MACHINE, "EXEC"):
        if marker not in output:
            raise RuntimeError(f"musl smoke ELF lacks {marker!r}")
    headers = [line.split() for line in output.splitlines()]
    if any(columns and columns[0] in {"INTERP", "DYNAMIC"} for columns in headers):
        raise RuntimeError("musl smoke must remain a static ET_EXEC")
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

    dynamic = run(
        [str(readelf), "--file-header", "--program-headers", "--dynamic", "--wide", str(binary.parent / "fs-root-dynamic")], ROOT
    )
    for marker in ("ELF64", ELF_MACHINE, "EXEC", f"[Requesting program interpreter: {TARGET.musl_loader}]"):
        if marker not in dynamic:
            raise RuntimeError(f"musl dynamic probe ELF lacks {marker!r}")
    needed = [line.split("[", 1)[1].split("]", 1)[0] for line in dynamic.splitlines() if "(NEEDED)" in line and "[" in line]
    if needed != ["libc.so"]:
        raise RuntimeError(f"musl dynamic probe must require bundled libc.so, got {needed!r}")


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
    from ext4_image import run_debugfs
    # Consumer generation owns the static main, real dynamic probe and matching libc.
    # Installing all three is required for the rooted PT_INTERP regression in full smoke.
    for directory in ("/lib", "/usr", "/usr/lib"):
        run_debugfs(image, f"mkdir {directory}", writable=True)
    for source, destination in ((binary.parent / "fs-root-dynamic", "/bin/fs-root-dynamic"),
                                (binary.parent / "libc.so", "/usr/lib/libc.so")):
        run_debugfs(image, f"write {source} {destination}", writable=True)
        run_debugfs(image, f"set_inode_field {destination} mode 0100755", writable=True)
    run_debugfs(image, f"symlink {TARGET.musl_loader} /usr/lib/libc.so", writable=True)
    # io_uring 的冷读用例需要“本次启动里从未被读过”的 ext4 文件，才能走 page cache 未命中的异步填页；
    # 内容公式与 io-uring.c 的 cold_byte 一致，其中一整页为零（hole）。
    for name, size, hole in (("a", 5 * 4096 + 1234, 2), ("b", 8 * 4096, None),
                             ("c", 3 * 4096, None), ("d", 2 * 4096, None),
                             ("e", 3 * 4096, None)):
        data = bytearray(((i * 131) + (i >> 12) * 17 + 7) & 0xFF for i in range(size))
        if hole is not None:
            data[hole * 4096:(hole + 1) * 4096] = bytes(4096)
        source = WORK / f"uring-cold-{name}.bin"
        source.write_bytes(data)
        run_debugfs(image, f"write {source} /usr/lib/uring-cold-{name}.bin", writable=True)
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
            linux_source = obtain_linux_source()
            install, sysroot_fingerprint = build_musl(
                source, linux_source, compiler, jobs_override, args.rebuild
            )
            binary = link_smoke(install, compiler, sysroot_fingerprint, args.rebuild)
            verify_elf(binary, compiler)
        if args.build_only:
            print(f"musl {MUSL_VERSION} runtime build passed")
            return 0
        stamp = ROOT / f"target/verify-gates/musl-{TARGET.arch}.json"
        runtime_inputs = [
            ROOT / f"target/{TARGET.kernel_triple}/release/kernel",
            binary,
            binary.parent / "fs-root-dynamic",
            binary.parent / "libc.so",
            ROOT / "create_fs.py",
            Path(__file__).resolve(),
            ROOT / "scripts/qemu_gate.py",
        ]
        if TARGET.requires_bootloader:
            runtime_inputs.insert(
                1,
                ROOT
                / f"bootloader/target/{TARGET.kernel_triple}/release/bootloader",
            )
        payload = runtime_gate_payload(
            "musl-runtime",
            5,
            tuple(runtime_inputs),
        )
        image = WORK / "fs.img"
        if runtime_gate_hit(stamp, payload, (image,)):
            print(f"musl {MUSL_VERSION} pthread signal swap verification cache hit")
            return 0
        image = create_image(binary)
        for cpu_count in (1, 2):
            boot(
                image,
                cpu_count,
                (
                    *cpu_topology_markers(cpu_count),
                    "LiteOS musl mmap placement ok",
                    "LiteOS musl RAW255 header ownership and send only delivery ok",
                    "LiteOS musl ASID ok",
                    "LiteOS musl raw thread credentials and NPROC ok",
                    "LiteOS musl fs context sharing and unshare ok",
                    "LiteOS musl file table sharing exec and POSIX close race ok",
                    "LiteOS musl thread mm lifetime nonleader exec identity and proc views ok",
                    "LiteOS musl capabilities thread ownership UID exec and zombie query ok",
                    "LiteOS musl sighand sharing exec detach pending discard and one shot ok",
                    "LiteOS musl fs root chroot lookup exec and proc views ok",
                    "LiteOS musl filesystem IDs dumpability exec proc and parent death ok",
                    "LiteOS musl anonymous inode identity reopen metadata session and statfs ok",
                    "LiteOS musl libc thread credential synchronization ok",
                    "LiteOS musl credential leader retirement ok",
                    "LiteOS musl pthread signal ok",
                    "LiteOS musl consumer exit=0",
                ),
                forbidden_markers=("LiteOS musl consumer failed", "LiteOS musl ASID failed", "LiteOS musl mmap placement failed", " fail step="),
            )
        with tempfile.TemporaryDirectory(prefix="liteos-swap-") as directory:
            scratch = Path(directory) / "swap.img"
            # Linux v1 4KiB header，磁盘格式与坏页/边界裁决另由生产 parser host 回归覆盖。
            pages = 64 * 1024 * 1024 // 4096
            header = bytearray(4096)
            struct.pack_into("<III", header, 1024, 1, pages - 1, 0)
            header[4086:] = b"SWAPSPACE2"
            with scratch.open("wb") as disk:
                disk.write(header)
                disk.truncate(pages * 4096)
            for cpu_count in (1, 2):
                boot(
                    image, cpu_count,
                    (*cpu_topology_markers(cpu_count), "LiteOS musl swapoff EINTR retry ok", "LiteOS musl swap ok", "LiteOS musl consumer exit=0"),
                    timeout_seconds=120,
                    forbidden_markers=("LiteOS musl swap failed", "LiteOS musl consumer failed"),
                    memory="128M",
                    kernel_arguments="init=/bin/init -- swap-smoke",
                    scratch_disk=scratch,
                )
        publish_runtime_gate(stamp, payload)
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"musl verification failed: {error}", file=sys.stderr)
        return 1
    print(f"musl {MUSL_VERSION} pthread signal swap verification passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
