#!/usr/bin/env python3
"""Validate the architecture-relevant properties of the three linked ELF images."""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def llvm_readelf() -> str:
    candidates = (
        shutil.which("llvm-readelf"),
        shutil.which("rust-llvm-readelf"),
        "/opt/homebrew/opt/llvm/bin/llvm-readelf",
    )
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return candidate
    raise RuntimeError("llvm-readelf is required by make verify")


def inspect(tool: str, relative: str) -> str:
    image = ROOT / relative
    if not image.is_file():
        raise RuntimeError(f"missing linked image: {relative}")
    return subprocess.run(
        [tool, "--file-header", "--program-headers", str(image)],
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


def main() -> int:
    try:
        tool = llvm_readelf()
        images = {
            "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader": (
                "ELF64",
                "RISC-V",
                "EXEC",
            ),
            "target/riscv64gc-unknown-none-elf/debug/kernel": ("ELF64", "RISC-V", "EXEC"),
            "target/riscv64gc-unknown-none-elf/release/init": (
                "ELF64",
                "RISC-V",
                "EXEC",
                "GNU_STACK",
            ),
        }
        for relative, markers in images.items():
            require(inspect(tool, relative), relative, markers)
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"artifact verification failed: {error}", file=sys.stderr)
        return 1
    print("artifact verification passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
