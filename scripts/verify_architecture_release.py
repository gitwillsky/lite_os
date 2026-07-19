#!/usr/bin/env python3
"""Verify target-specific static façade and ISA containment in the release ELF."""

from __future__ import annotations

import re
import shutil
import subprocess
import sys
from pathlib import Path

from build_target import target_from_environment

ROOT = Path(__file__).resolve().parent.parent
FACADE_SYMBOLS = (
    "kernel::arch::interrupt::",
    "kernel::arch::context::",
    "kernel::arch::cpu::",
    "kernel::arch::time::",
    "kernel::arch::mmu::",
    "kernel::arch::trap::",
    "kernel::arch::user::",
    "kernel::platform::initialize",
    "kernel::platform::initialize_devices",
    "kernel::platform::claim_interrupt",
    "kernel::platform::complete_interrupt",
    "kernel::platform::send_ipi",
    "kernel::platform::synchronize_tlb",
    "kernel::platform::synchronize_instruction_cache",
)
DYNAMIC_ARCHITECTURE_MARKERS = (
    "dyn Architecture",
    "trait Architecture",
    "vtable for Architecture",
    "dyn Platform",
    "trait Platform",
    "vtable for Platform",
)

AARCH64_FP_SIMD_REGISTER = re.compile(
    r"\b(?:[bhsdqv](?:[0-9]|[12][0-9]|3[01])(?:\.[0-9]+[bhsd])?|"
    r"z(?:[0-9]|[12][0-9]|3[01])(?:\.[bhsdq])?|"
    r"p(?:[0-9]|1[0-5])(?:\.[bhsd])?|za(?:[0-9]+[hv]?)?(?:\.[bhsdq])?|"
    r"ffr|fpcr|fpsr)\b",
    re.IGNORECASE,
)
AARCH64_SCALABLE_REGISTER = re.compile(
    r"\b(?:z(?:[0-9]|[12][0-9]|3[01])(?:\.[bhsdq])?|"
    r"p(?:[0-9]|1[0-5])(?:\.[bhsd])?|za(?:[0-9]+[hv]?)?(?:\.[bhsdq])?|ffr)\b",
    re.IGNORECASE,
)
AARCH64_FORBIDDEN_INSTRUCTION = re.compile(
    r"^(?:"
    # Pointer authentication is forbidden even when emitted as a HINT-compatible alias.
    r"pac[a-z0-9]*|aut[a-z0-9]*|xpac[a-z0-9]*|retaa|retab|eretaa|eretab|"
    r"braa|braaz|brab|brabz|blraa|blraaz|blrab|blrabz|ldraa|ldrab|"
    # Memory tagging changes the kernel pointer/memory contract.
    r"irg|gmi|subp|subps|addg|subg|ldg|stg|stzg|st2g|stz2g|stgp|"
    r"ldgm|stgm|stzgm|"
    # SVE/SME scalar vector-length and streaming-mode instructions have no Z/P/ZA operand.
    r"addpl|addvl|rdvl|rdsvl|cnt[bhwd]|inc[bhwd]|dec[bhwd]|"
    r"sqinc[bhwd]|uqinc[bhwd]|sqdec[bhwd]|uqdec[bhwd]|smstart|smstop"
    r")$",
    re.IGNORECASE,
)
AARCH64_FORBIDDEN_OPERAND = re.compile(
    r"\b(?:gva|gzva|tco|svcr(?:sm|za)?)\b", re.IGNORECASE
)
AARCH64_FP_OWNER_SYMBOLS = {
    "__switch",
    "__aarch64_signal_fp_capture",
    "__aarch64_signal_fp_restore",
    "__aarch64_clone_fp_capture",
    "__aarch64_exec_fp_reset",
}


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


def run(tool: str, kernel: Path, *arguments: str) -> str:
    return subprocess.run(
        [tool, *arguments, str(kernel)],
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


def aarch64_instructions(disassembly: str) -> list[tuple[str, str, str]]:
    """Return ``(symbol, mnemonic, instruction)`` records from llvm-objdump output."""
    symbol = "<no-symbol>"
    records = []
    for line in disassembly.splitlines():
        header = re.match(r"^[0-9a-fA-F]+ <(.+)>:$", line)
        if header:
            symbol = header.group(1)
            continue
        decoded = re.match(r"^\s*[0-9a-fA-F]+:\s+(.+)$", line)
        if not decoded:
            continue
        instruction = " ".join(decoded.group(1).split()).lower()
        raw_data = re.match(r"^(?:[0-9a-f]{2}\s+){4}(.+)$", instruction)
        if raw_data:
            instruction = raw_data.group(1)
        mnemonic = instruction.split(maxsplit=1)[0]
        if mnemonic.startswith("."):
            continue
        records.append((symbol, mnemonic, instruction))
    return records


def verify_aarch64_instruction_containment(disassembly: str) -> None:
    """Reject accidental use of optional AArch64 execution state in kernel code.

    ``__switch`` is the sole owner of Q0-Q31 and FPCR/FPSR save/restore. The
    exact context shape is separately ratcheted by ``check_trap_cost.py``.
    """
    for symbol, mnemonic, instruction in aarch64_instructions(disassembly):
        if "<unknown>" in instruction:
            raise RuntimeError(
                f"AArch64 disassembly contains an undecoded instruction in {symbol}"
            )
        if AARCH64_FORBIDDEN_INSTRUCTION.fullmatch(mnemonic):
            raise RuntimeError(
                f"AArch64 release ELF uses forbidden {mnemonic} instruction in {symbol}"
            )
        if AARCH64_FORBIDDEN_OPERAND.search(instruction):
            raise RuntimeError(
                f"AArch64 release ELF uses forbidden optional ISA state in {symbol}: "
                f"{instruction}"
            )
        if AARCH64_SCALABLE_REGISTER.search(instruction):
            raise RuntimeError(
                f"AArch64 release ELF uses forbidden SVE/SME state in {symbol}: "
                f"{instruction}"
            )
        if AARCH64_FP_SIMD_REGISTER.search(instruction) and symbol not in AARCH64_FP_OWNER_SYMBOLS:
            raise RuntimeError(
                f"AArch64 FP/NEON state escapes explicit boundary helpers in {symbol}: "
                f"{instruction}"
            )


def main() -> int:
    try:
        target = target_from_environment()
        kernel = ROOT / "target" / target.kernel_triple / "release/kernel"
        if not kernel.is_file():
            raise RuntimeError(f"missing release kernel: {kernel.relative_to(ROOT)}")
        symbols = run(llvm_tool("llvm-nm"), kernel, "--demangle", "--defined-only")
        disassembly = run(
            llvm_tool("llvm-objdump"),
            kernel,
            "--demangle",
            "--no-show-raw-insn",
            "-d",
        )

        # 1. A selected backend must survive into the target ELF, or the gate inspected
        #    the wrong artifact and its negative checks would be meaningless.
        backend_marker = f"kernel::arch::{target.arch}::"
        if backend_marker not in symbols or backend_marker not in disassembly:
            raise RuntimeError(
                f"release ELF lacks the selected {target.arch} backend marker"
            )
        other_architecture = "riscv64" if target.arch == "aarch64" else "aarch64"
        reject_markers(
            "selected release ELF",
            symbols + disassembly,
            (f"kernel::arch::{other_architecture}::",),
        )

        # 2. Static re-exports do not create façade wrapper symbols. A wrapper here would
        #    add a call boundary and make later runtime dispatch easy to hide.
        reject_markers("symbol table", symbols, FACADE_SYMBOLS)
        reject_markers("disassembly", disassembly, FACADE_SYMBOLS)

        # 3. Architecture trait objects or vtables are incompatible with the compile-time
        #    backend contract even if a benchmark happens to inline a particular caller.
        reject_markers("symbol table", symbols, DYNAMIC_ARCHITECTURE_MARKERS)
        reject_markers("disassembly", disassembly, DYNAMIC_ARCHITECTURE_MARKERS)

        if target.arch == "aarch64":
            verify_aarch64_instruction_containment(disassembly)
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"architecture release gate failed: {error}", file=sys.stderr)
        return 1
    print("architecture release gate passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
