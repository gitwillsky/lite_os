#!/usr/bin/env python3
"""Verify that the selected architecture façade remains static in the target ELF."""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
KERNEL = ROOT / "target/riscv64gc-unknown-none-elf/release/kernel"
FACADE_SYMBOLS = (
    "kernel::arch::interrupt::",
    "kernel::arch::context::",
    "kernel::arch::cpu::",
    "kernel::arch::time::",
    "kernel::arch::mmu::",
    "kernel::arch::trap::",
    "kernel::arch::user::",
)
DYNAMIC_ARCHITECTURE_MARKERS = (
    "dyn Architecture",
    "trait Architecture",
    "vtable for Architecture",
)


def llvm_tool(name: str) -> str:
    candidates = (
        shutil.which(name),
        shutil.which(f"rust-{name}"),
        f"/opt/homebrew/opt/llvm/bin/{name}",
    )
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return candidate
    raise RuntimeError(f"{name} is required by the architecture release gate")


def run(tool: str, *arguments: str) -> str:
    return subprocess.run(
        [tool, *arguments, str(KERNEL)],
        check=True,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    ).stdout


def reject_markers(label: str, output: str, markers: tuple[str, ...]) -> None:
    for marker in markers:
        if marker in output:
            raise RuntimeError(f"{label} exposes forbidden architecture marker {marker!r}")


def main() -> int:
    try:
        if not KERNEL.is_file():
            raise RuntimeError(f"missing release kernel: {KERNEL.relative_to(ROOT)}")
        symbols = run(llvm_tool("llvm-nm"), "--demangle", "--defined-only")
        disassembly = run(llvm_tool("llvm-objdump"), "--demangle", "-d")

        # 1. A selected backend must survive into the target ELF, or the gate inspected
        #    the wrong artifact and its negative checks would be meaningless.
        backend_marker = "kernel::arch::riscv64::"
        if backend_marker not in symbols or backend_marker not in disassembly:
            raise RuntimeError("release ELF lacks the selected RISC-V64 backend marker")

        # 2. Static re-exports do not create façade wrapper symbols. A wrapper here would
        #    add a call boundary and make later runtime dispatch easy to hide.
        reject_markers("symbol table", symbols, FACADE_SYMBOLS)
        reject_markers("disassembly", disassembly, FACADE_SYMBOLS)

        # 3. Architecture trait objects or vtables are incompatible with the compile-time
        #    backend contract even if a benchmark happens to inline a particular caller.
        reject_markers("symbol table", symbols, DYNAMIC_ARCHITECTURE_MARKERS)
        reject_markers("disassembly", disassembly, DYNAMIC_ARCHITECTURE_MARKERS)
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"architecture release gate failed: {error}", file=sys.stderr)
        return 1
    print("architecture release gate passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
