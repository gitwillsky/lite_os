"""固定 rust-src LLVM libunwind 的构建与不可变发布。"""
from __future__ import annotations

import shutil
from build_cache import (
    fingerprint,
    generation_directory,
    manifest_matches,
    publish_generation,
    sha256,
    write_manifest,
)
from build_target import target_from_environment
from pathlib import Path
from toolchain.musl import MuslCachePaths, run, rustc_probe_environment

ROOT = Path(__file__).resolve().parents[2]
TARGET = target_from_environment()
WORK = ROOT / "target" / "rust-std-runtime" / TARGET.arch

LIBUNWIND_CXX_SOURCES = ("libunwind.cpp", "Unwind-EHABI.cpp", "Unwind-seh.cpp")

LIBUNWIND_C_SOURCES = (
    "UnwindLevel1.c",
    "UnwindLevel1-gcc-ext.c",
    "Unwind-sjlj.c",
    "Unwind-wasm.c",
)

LIBUNWIND_ASM_SOURCES = ("UnwindRegistersRestore.S", "UnwindRegistersSave.S")


def rust_source_root(rustc: str) -> Path:
    """返回固定 toolchain 的完整 rust-src root；缺失时禁止回退 host LLVM。"""
    # Make jobserver fd 不属于 rustc；若继承失效 capability，其 warning 会和被捕获的 stdout
    # 合并并把 sysroot 解析成不存在的多行路径。
    sysroot = Path(
        run(
            [rustc, "--print", "sysroot"],
            ROOT,
            rustc_probe_environment(),
        ).strip()
    )
    source = sysroot / "lib/rustlib/src/rust"
    if not (source / "library/std/Cargo.toml").is_file():
        raise RuntimeError("pinned rust-src component is required for Rust std userspace")
    return source


def build_libunwind(musl: MuslCachePaths, rustc: str) -> Path:
    """从同一固定 rust-src 构建目标静态 LLVM libunwind。"""
    rust_source = rust_source_root(rustc)
    source = rust_source / "src/llvm-project/libunwind"
    source_directory = source / "src"
    selected_sources = tuple(
        source_directory / name
        for name in (
            *LIBUNWIND_CXX_SOURCES,
            *LIBUNWIND_C_SOURCES,
            *LIBUNWIND_ASM_SOURCES,
        )
    )
    headers = tuple(
        sorted(
            path
            for directory in (source / "include", source_directory)
            for path in directory.rglob("*")
            if path.is_file() and path.suffix in {".h", ".hpp"}
        )
    )
    for path in (*selected_sources, *headers):
        if not path.is_file():
            raise RuntimeError(f"pinned rust-src libunwind input is missing: {path}")
    payload = {
        "kind": "rust-std-libunwind",
        "recipe_version": 1,
        "arch": TARGET.arch,
        "linux_target": TARGET.linux_triple,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "compiler": run([str(musl.compiler), "--version"], ROOT).splitlines()[0],
        "archiver": run([str(musl.archiver), "--version"], ROOT).splitlines()[0],
        "source_sha256": {
            str(path.relative_to(source)): sha256(path)
            for path in (*selected_sources, *headers)
        },
    }
    identity = fingerprint(payload)
    entry = WORK / "libunwind" / identity
    if manifest_matches(entry, payload, ("libunwind.a",)):
        return entry / "libunwind.a"

    generation = generation_directory(WORK / "libunwind-generations", identity)
    common = [
        str(musl.compiler),
        f"--target={TARGET.linux_triple}",
        "-nostdlibinc",
        "-isystem",
        str(musl.install / "usr/include"),
        "-I",
        str(source / "include"),
        "-I",
        str(source_directory),
        "-D_LIBUNWIND_IS_NATIVE_ONLY",
        "-DNDEBUG",
        # libunwind 只服务 panic/backtrace 冷路径；release 固定 O2，正确性由 std runtime gate
        # 验证，不增加无法代表 target unwind 的 host wall-clock benchmark。
        "-O2",
        "-fPIC",
        "-ffunction-sections",
        "-fdata-sections",
        "-funwind-tables",
        "-fvisibility=hidden",
    ]
    if TARGET.arch == "aarch64":
        common.append("-march=armv8-a")
    else:
        common.extend(("-march=rv64gc", "-mabi=lp64d"))
    objects: list[Path] = []
    try:
        for index, path in enumerate(selected_sources):
            output = generation / f"{index}-{path.name}.o"
            if path.name in LIBUNWIND_CXX_SOURCES:
                language = (
                    "-x",
                    "c++",
                    "-std=c++17",
                    "-fno-exceptions",
                    "-fno-rtti",
                    "-nostdinc++",
                )
            elif path.name in LIBUNWIND_C_SOURCES:
                language = ("-x", "c", "-std=c99", "-fexceptions")
            else:
                language = ("-x", "assembler-with-cpp")
            run([*common, *language, "-c", str(path), "-o", str(output)], ROOT)
            objects.append(output)
        archive = generation / "libunwind.a"
        run([str(musl.archiver), "crs", str(archive), *(str(path) for path in objects)], ROOT)
        run([str(musl.ranlib), str(archive)], ROOT)
        for path in objects:
            path.unlink()
        write_manifest(generation, payload)
        publish_generation(generation, entry)
    except BaseException:
        shutil.rmtree(generation, ignore_errors=True)
        raise
    return entry / "libunwind.a"
