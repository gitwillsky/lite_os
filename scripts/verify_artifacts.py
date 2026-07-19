#!/usr/bin/env python3
"""Validate target-specific ELF and AArch64 Image boot contracts."""

from __future__ import annotations

import argparse
import os
import re
import shutil
import struct
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from build_target import BuildTarget, target_from_environment
from verify_busybox import cached_busybox_binary
from verify_musl import find_compiler

ROOT = Path(__file__).resolve().parent.parent
PT_LOAD = 1
PF_X = 1
AARCH64_MACHINE = 183
AARCH64_TEXT_OFFSET = 0x80000
AARCH64_PLACEMENT_ALIGNMENT = 0x200000
AARCH64_IMAGE_MAGIC = b"ARM\x64"


@dataclass(frozen=True)
class ArtifactSpec:
    path: Path
    markers: tuple[str, ...]


@dataclass(frozen=True)
class ElfLoadSegment:
    """一个 ELF PT_LOAD 的文件与物理内存投影。"""

    file_offset: int
    virtual: int
    physical: int
    file_size: int
    memory_size: int
    flags: int


@dataclass(frozen=True)
class AArch64ElfLayout:
    """raw Image generation 所需的完整 AArch64 ELF LMA 契约。"""

    entry: int
    entry_physical: int
    base_physical: int
    load_end_physical: int
    end_physical: int
    text_offset: int
    image_size: int
    magic: bytes
    segments: tuple[ElfLoadSegment, ...]


def llvm_tool(name: str) -> str:
    candidates = (
        shutil.which(name),
        shutil.which(f"rust-{name.removeprefix('llvm-')}"),
        f"/opt/homebrew/opt/llvm/bin/{name}",
    )
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return candidate
    raise RuntimeError(f"{name} is required by make verify")


def inspect(tool: str, image: Path) -> str:
    if not image.is_file():
        raise RuntimeError(f"missing linked image: {image}")
    return subprocess.run(
        [tool, "--file-header", "--program-headers", "--dynamic", str(image)],
        check=True,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    ).stdout


def require(output: str, relative: str, markers: tuple[str, ...]) -> None:
    for marker in markers:
        if marker not in output:
            raise RuntimeError(f"{relative}: llvm-readelf output lacks {marker!r}")
    for line in output.splitlines():
        columns = line.split()
        if len(columns) < 8 or columns[0] not in {"LOAD", "GNU_STACK"}:
            continue
        flags = "".join(columns[6:-1])
        if columns[0] == "LOAD" and "W" in flags and "E" in flags:
            raise RuntimeError(f"{relative}: writable executable LOAD segment: {line.strip()}")
        if columns[0] == "GNU_STACK" and "E" in flags:
            raise RuntimeError(f"{relative}: executable GNU_STACK: {line.strip()}")


def target_artifacts(target: BuildTarget, busybox: Path) -> tuple[ArtifactSpec, ...]:
    """Return the exact release products owned by one selected target."""
    machine = "AArch64" if target.arch == "aarch64" else "RISC-V"
    artifacts = [
        ArtifactSpec(
            ROOT / target.kernel_elf(),
            ("ELF64", machine, "EXEC"),
        )
    ]
    if target.requires_bootloader:
        artifacts.append(
            ArtifactSpec(
                ROOT
                / "bootloader/target"
                / target.kernel_triple
                / "release/bootloader",
                ("ELF64", "RISC-V", "EXEC"),
            )
        )
    artifacts.append(
        ArtifactSpec(
            busybox,
            (
                "ELF64",
                machine,
                "DYN (",
                "INTERP",
                "DYNAMIC",
                "GNU_RELRO",
                "GNU_STACK",
                target.musl_loader,
            ),
        )
    )
    return tuple(artifacts)


def symbol_addresses(output: str) -> dict[str, int]:
    addresses = {}
    for line in output.splitlines():
        match = re.match(r"^([0-9a-fA-F]+)\s+\S\s+(.+)$", line.strip())
        if match:
            addresses[match.group(2)] = int(match.group(1), 16)
    return addresses


def aarch64_elf_layout(image: Path) -> AArch64ElfLayout:
    """解析 ELF PT_LOAD LMA 与入口处 Linux arm64 Image header。"""
    if not image.is_file():
        raise RuntimeError(f"missing AArch64 kernel ELF: {image}")
    data = image.read_bytes()
    elf_format = "<16sHHIQQQIHHHHHH"
    if len(data) < struct.calcsize(elf_format):
        raise RuntimeError(f"{image}: truncated ELF header")
    header = struct.unpack_from(elf_format, data)
    identity, elf_type, machine, _, entry, phoff, _, _, _, phentsize, phnum, *_ = header
    if identity[:4] != b"\x7fELF" or identity[4:6] != b"\x02\x01":
        raise RuntimeError(f"{image}: expected little-endian ELF64")
    if elf_type != 2 or machine != AARCH64_MACHINE:
        raise RuntimeError(f"{image}: expected AArch64 ET_EXEC")

    program_format = "<IIQQQQQQ"
    if phentsize < struct.calcsize(program_format):
        raise RuntimeError(f"{image}: invalid ELF program-header size")
    entry_file_offset: int | None = None
    entry_physical: int | None = None
    segments = []
    for index in range(phnum):
        offset = phoff + index * phentsize
        if offset + struct.calcsize(program_format) > len(data):
            raise RuntimeError(f"{image}: truncated ELF program headers")
        kind, flags, file_offset, virtual, physical, file_size, memory_size, _ = (
            struct.unpack_from(program_format, data, offset)
        )
        if kind == PT_LOAD:
            if file_size > memory_size or file_offset + file_size > len(data):
                raise RuntimeError(f"{image}: invalid PT_LOAD file range")
            if memory_size > 0:
                segments.append(
                    ElfLoadSegment(
                        file_offset=file_offset,
                        virtual=virtual,
                        physical=physical,
                        file_size=file_size,
                        memory_size=memory_size,
                        flags=flags,
                    )
                )
        if (
            kind == PT_LOAD
            and flags & PF_X
            and virtual <= entry
            and entry + 64 <= virtual + file_size
            and entry_file_offset is None
        ):
            delta = entry - virtual
            entry_file_offset = file_offset + delta
            entry_physical = physical + delta
    if entry_file_offset is None or entry_physical is None:
        raise RuntimeError(f"{image}: entry is not backed by an executable LOAD segment")
    if entry_file_offset + 64 > len(data):
        raise RuntimeError(f"{image}: truncated arm64 Image header")

    text_offset, image_size = struct.unpack_from("<QQ", data, entry_file_offset + 8)
    magic = data[entry_file_offset + 56 : entry_file_offset + 60]
    ordered = tuple(sorted(segments, key=lambda segment: segment.physical))
    if not ordered:
        raise RuntimeError(f"{image}: AArch64 ELF has no loadable memory")
    previous_end = ordered[0].physical
    for segment in ordered:
        if segment.physical < previous_end:
            raise RuntimeError(f"{image}: overlapping PT_LOAD physical ranges")
        previous_end = segment.physical + segment.memory_size
    base_physical = ordered[0].physical
    load_end_physical = max(
        segment.physical + segment.memory_size for segment in ordered
    )
    if entry_physical != base_physical:
        raise RuntimeError(
            f"{image}: arm64 Image header is not at the first PT_LOAD LMA"
        )
    if text_offset != AARCH64_TEXT_OFFSET:
        raise RuntimeError(
            f"{image}: arm64 Image text_offset is {text_offset:#x}, "
            f"expected {AARCH64_TEXT_OFFSET:#x}"
        )
    if magic != AARCH64_IMAGE_MAGIC:
        raise RuntimeError(f"{image}: arm64 Image magic is {magic!r}")
    if entry_physical % AARCH64_PLACEMENT_ALIGNMENT != AARCH64_TEXT_OFFSET:
        raise RuntimeError(
            f"{image}: physical entry {entry_physical:#x} violates arm64 Image placement"
        )
    end_physical = base_physical + image_size
    if image_size <= 0 or load_end_physical > end_physical:
        raise RuntimeError(
            f"{image}: arm64 Image header ends at {end_physical:#x} before "
            f"PT_LOAD memory end {load_end_physical:#x}"
        )
    return AArch64ElfLayout(
        entry=entry,
        entry_physical=entry_physical,
        base_physical=base_physical,
        load_end_physical=load_end_physical,
        end_physical=end_physical,
        text_offset=text_offset,
        image_size=image_size,
        magic=magic,
        segments=ordered,
    )


def require_aarch64_elf_contract(image: Path, nm_output: str) -> AArch64ElfLayout:
    """验证 ELF header、LMA extent 与导出的物理 Image boundary symbols。"""
    layout = aarch64_elf_layout(image)
    symbols = symbol_addresses(nm_output)
    required = ("_start", "skernel", "ekernel", "kernel_image_end_phys")
    missing = [name for name in required if name not in symbols]
    if missing:
        raise RuntimeError(f"{image}: missing image boundary symbols: {missing!r}")
    if symbols["_start"] != layout.entry:
        raise RuntimeError(f"{image}: ELF entry does not match _start")
    if symbols["skernel"] >= symbols["ekernel"]:
        raise RuntimeError(f"{image}: invalid skernel/ekernel order")
    if symbols["kernel_image_end_phys"] != layout.end_physical:
        raise RuntimeError(
            f"{image}: kernel_image_end_phys {symbols['kernel_image_end_phys']:#x} "
            f"!= PT_LOAD end {layout.end_physical:#x}"
        )
    return layout


def expected_aarch64_raw_image(image: Path, layout: AArch64ElfLayout) -> bytes:
    """按 PT_LOAD LMA 构造 raw Image 的唯一字节布局。"""
    elf = image.read_bytes()
    expected = bytearray(layout.image_size)
    for segment in layout.segments:
        if segment.file_size == 0:
            continue
        start = segment.physical - layout.base_physical
        end = start + segment.file_size
        expected[start:end] = elf[
            segment.file_offset : segment.file_offset + segment.file_size
        ]
    return bytes(expected)


def require_aarch64_raw_image(kernel_elf: Path, image: Path) -> None:
    """验证 raw Image header、完整大小与 ELF PT_LOAD LMA 字节布局。"""
    layout = aarch64_elf_layout(kernel_elf)
    if not image.is_file():
        raise RuntimeError(f"missing AArch64 raw Image: {image}")
    raw = image.read_bytes()
    if len(raw) != layout.image_size:
        raise RuntimeError(
            f"{image}: raw Image size {len(raw):#x} != header size "
            f"{layout.image_size:#x}"
        )
    if len(raw) < 64:
        raise RuntimeError(f"{image}: truncated raw arm64 Image header")
    text_offset, image_size = struct.unpack_from("<QQ", raw, 8)
    magic = raw[56:60]
    if (text_offset, image_size, magic) != (
        layout.text_offset,
        layout.image_size,
        layout.magic,
    ):
        raise RuntimeError(f"{image}: raw arm64 Image header differs from ELF")
    expected = expected_aarch64_raw_image(kernel_elf, layout)
    if raw != expected:
        mismatch = next(
            index for index, (actual, wanted) in enumerate(zip(raw, expected))
            if actual != wanted
        )
        raise RuntimeError(
            f"{image}: raw Image differs from ELF LMA layout at offset {mismatch:#x}"
        )


def run_host(command: list[str]) -> str:
    """执行 artifact tool，并把失败命令与输出保留在单一错误中。"""
    result = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-40:])
        raise RuntimeError(f"command failed: {' '.join(command)}\n{tail}")
    return result.stdout


def pinned_rust_objcopy() -> Path:
    """返回当前固定 Rust toolchain 自带的 llvm-objcopy。"""
    rustc = shutil.which("rustc")
    if rustc is None:
        raise RuntimeError("rustc is required to locate pinned Rust llvm-objcopy")
    sysroot = Path(run_host([rustc, "--print", "sysroot"]).strip())
    verbose = run_host([rustc, "-vV"])
    host = next(
        (line.removeprefix("host: ") for line in verbose.splitlines() if line.startswith("host: ")),
        None,
    )
    if host is None:
        raise RuntimeError("rustc -vV did not report the pinned host triple")
    directory = sysroot / "lib" / "rustlib" / host / "bin"
    for name in ("llvm-objcopy", "rust-objcopy"):
        candidate = directory / name
        if candidate.is_file():
            return candidate.resolve()
    raise RuntimeError(f"pinned Rust llvm-objcopy is missing under {directory}")


def build_kernel_boot_artifact(target: BuildTarget, profile: str) -> Path:
    """从 kernel ELF 原子生成当前架构的 QEMU boot artifact。"""
    kernel_elf = ROOT / target.kernel_elf(profile)
    boot_artifact = ROOT / target.kernel_boot_artifact(profile)
    if not kernel_elf.is_file():
        raise RuntimeError(f"kernel ELF is missing: {kernel_elf}")
    if not target.requires_raw_kernel_image:
        if boot_artifact != kernel_elf:
            raise RuntimeError("non-raw target boot artifact must be its kernel ELF")
        return kernel_elf

    layout = aarch64_elf_layout(kernel_elf)
    temporary = boot_artifact.parent / f".{boot_artifact.name}.{os.getpid()}.tmp"
    temporary.unlink(missing_ok=True)
    try:
        objcopy = pinned_rust_objcopy()
        run_host([str(objcopy), "-O", "binary", str(kernel_elf), str(temporary)])
        if not temporary.is_file():
            raise RuntimeError("pinned Rust llvm-objcopy did not create a raw Image")
        if temporary.stat().st_size > layout.image_size:
            raise RuntimeError(
                f"objcopy output exceeds arm64 Image header size: {temporary}"
            )
        with temporary.open("r+b") as output:
            output.truncate(layout.image_size)
        require_aarch64_raw_image(kernel_elf, temporary)
        os.replace(temporary, boot_artifact)
    except Exception:
        temporary.unlink(missing_ok=True)
        raise
    return boot_artifact


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-boot-artifact", action="store_true")
    parser.add_argument("--profile", choices=("release", "debug"), default="release")
    arguments = parser.parse_args()
    try:
        target = target_from_environment()
        if arguments.build_boot_artifact:
            artifact = build_kernel_boot_artifact(target, arguments.profile)
            print(f"kernel boot artifact ready: {artifact.relative_to(ROOT)}")
            return 0
        readelf = llvm_tool("llvm-readelf")
        busybox = cached_busybox_binary(find_compiler())
        artifacts = target_artifacts(target, busybox)
        for artifact in artifacts:
            label = (
                str(artifact.path.relative_to(ROOT))
                if artifact.path.is_relative_to(ROOT)
                else str(artifact.path)
            )
            require(inspect(readelf, artifact.path), label, artifact.markers)
        if target.arch == "aarch64":
            kernel = artifacts[0].path
            nm_output = subprocess.run(
                [llvm_tool("llvm-nm"), "--defined-only", str(kernel)],
                check=True,
                cwd=ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            ).stdout
            require_aarch64_elf_contract(kernel, nm_output)
            require_aarch64_raw_image(
                kernel,
                ROOT / target.kernel_boot_artifact(),
            )
    except (RuntimeError, OSError, struct.error, subprocess.CalledProcessError) as error:
        print(f"artifact verification failed: {error}", file=sys.stderr)
        return 1
    print("artifact verification passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
