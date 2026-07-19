#!/usr/bin/env python3
"""Gate target-specific release trap/context-switch instruction costs."""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from build_target import BuildTarget, target_from_environment


ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class Symbol:
    address: int
    size: int
    name: str


def llvm_objdump() -> str:
    candidates = (
        shutil.which("llvm-objdump"),
        shutil.which("rust-objdump"),
        "/opt/homebrew/opt/llvm/bin/llvm-objdump",
    )
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return candidate
    raise RuntimeError("llvm-objdump is required by the release trap-cost gate")


def run(command: list[str]) -> str:
    return subprocess.run(
        command,
        cwd=ROOT,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    ).stdout


def parse_symbols(output: str) -> list[Symbol]:
    parsed = []
    pattern = re.compile(
        r"^([0-9a-fA-F]+)\s+\S*\s*(?:F\s+)?\.text\s+([0-9a-fA-F]+)\s+(.+)$"
    )
    for line in output.splitlines():
        match = pattern.match(line)
        if match:
            parsed.append(
                Symbol(int(match.group(1), 16), int(match.group(2), 16), match.group(3))
            )
    return sorted(parsed, key=lambda symbol: (symbol.address, symbol.name))


def symbols(tool: str, elf: Path) -> list[Symbol]:
    return parse_symbols(run([tool, "-t", "-C", str(elf)]))


def find_symbol(table: list[Symbol], name: str) -> Symbol:
    return next(symbol for symbol in table if symbol.name == name)


def parse_instructions(output: str) -> list[str]:
    decoded = []
    for line in output.splitlines():
        match = re.match(
            r"^\s*[0-9a-fA-F]+:\s+(?:(?:[0-9a-fA-F]{2}\s+){2,8}|[0-9a-fA-F]{8}\s+)?(.+)$",
            line,
        )
        if match:
            decoded.append(" ".join(match.group(1).split()).lower())
    return decoded


def instructions(tool: str, elf: Path, table: list[Symbol], name: str) -> list[str]:
    symbol = find_symbol(table, name)
    stop = symbol.address + symbol.size
    if symbol.size == 0:
        stop = next(
            candidate.address for candidate in table if candidate.address > symbol.address
        )
    output = run(
        [
            tool,
            "-d",
            "-C",
            "--no-show-raw-insn",
            f"--start-address=0x{symbol.address:x}",
            f"--stop-address=0x{stop:x}",
            str(elf),
        ]
    )
    return parse_instructions(output)


def count_matching(lines: list[str], pattern: str) -> int:
    expression = re.compile(pattern)
    return sum(expression.search(line) is not None for line in lines)


def riscv_measurements(
    trap: list[str], user_return: list[str]
) -> tuple[dict[str, int], dict[str, int]]:
    """保留既有 RISC-V trap 热路径上限。"""
    measured = {
        "satp_writes": count_matching(trap, r"\bcsrw\s+satp\b"),
        "full_sfence_vma": count_matching(trap, r"^sfence\.vma$"),
        "fp_stores": count_matching(trap, r"^fs[wdq]\b"),
        "fp_loads": count_matching(trap, r"^fl[wdq]\b"),
        "fence_i": count_matching(user_return, r"^fence\.i$"),
    }
    return measured, {
        "satp_writes": 2,
        "full_sfence_vma": 0,
        "fp_stores": 0,
        "fp_loads": 0,
        "fence_i": 0,
    }


def q_registers(lines: list[str], operation: str) -> list[int]:
    registers = []
    for line in lines:
        mnemonic = line.split(maxsplit=1)[0]
        if not mnemonic.startswith(operation):
            continue
        registers.extend(int(value) for value in re.findall(r"\bq([0-9]|[12][0-9]|3[01])\b", line))
    return registers


def aarch64_measurements(
    ordinary_trap: list[str],
    user_trap_assembly: list[str],
    kernel_irq: list[str],
    bootstrap_wait: list[str],
    user_return: list[str],
    context_switch: list[str],
    signal_capture: list[str],
    signal_restore: list[str],
    clone_capture: list[str],
    exec_reset: list[str],
) -> tuple[dict[str, int], dict[str, int]]:
    """量化 AArch64 普通 trap 与唯一完整 FP context-switch seam。"""
    fp_register = re.compile(
        r"\b(?:[bhsdqv](?:[0-9]|[12][0-9]|3[01])|"
        r"z(?:[0-9]|[12][0-9]|3[01])|p(?:[0-9]|1[0-5])|fpcr|fpsr)\b"
    )
    ordinary_fp_load_store = sum(
        line.split(maxsplit=1)[0].startswith(("ld", "st"))
        and fp_register.search(line) is not None
        for line in ordinary_trap
    )
    saved = q_registers(context_switch, "st")
    restored = q_registers(context_switch, "ld")
    helper_registers = {
        "signal_capture": q_registers(signal_capture, "st"),
        "signal_restore": q_registers(signal_restore, "ld"),
        "clone_capture": q_registers(clone_capture, "st"),
        "exec_reset": q_registers(exec_reset, "ld"),
    }
    ttbr0_write = next(
        (index for index, line in enumerate(user_trap_assembly) if re.search(r"^msr\s+ttbr0_el1\b", line)),
        None,
    )
    vbar_write = next(
        (index for index, line in enumerate(user_trap_assembly) if re.search(r"^msr\s+vbar_el1\b", line)),
        None,
    )
    measured = {
        "ordinary_fp_load_store": ordinary_fp_load_store,
        "ordinary_q_instructions": count_matching(
            ordinary_trap, r"\bq(?:[0-9]|[12][0-9]|3[01])\b"
        ),
        "ordinary_fp_system": count_matching(ordinary_trap, r"\b(?:fpcr|fpsr)\b"),
        "user_contextidr_reads": count_matching(
            user_trap_assembly, r"^mrs\s+\w+,\s*contextidr_el1\b"
        ),
        "user_contextidr_writes": count_matching(
            user_trap_assembly, r"^msr\s+contextidr_el1\b"
        ),
        "user_context_stack_address_adds": count_matching(
            user_trap_assembly, r"^add\s+x11,\s*sp,\s*#(?:0x30|48)$"
        ),
        "user_context_metadata_loads": count_matching(
            user_trap_assembly,
            r"^ldr\s+x11,\s*\[sp(?:,\s*#(?:0x)?[0-9a-f]+)?\]$",
        ),
        "user_context_metadata_stores": count_matching(
            user_trap_assembly,
            r"^str\s+x16,\s*\[sp(?:,\s*#(?:0x)?0)?\]$",
        ),
        "user_tpidrro_reads": count_matching(
            user_trap_assembly, r"^mrs\s+\w+,\s*tpidrro_el0\b"
        ),
        "user_tpidrro_zero_writes": count_matching(
            user_trap_assembly, r"^msr\s+tpidrro_el0,\s*xzr$"
        ),
        "user_tpidr_el0_writes": count_matching(
            user_trap_assembly, r"^msr\s+tpidr_el0\b"
        ),
        "user_scratch_stack_reserves": count_matching(
            user_trap_assembly, r"^sub\s+sp,\s*sp,\s*#0x20$"
        ),
        "user_scratch_stack_releases": count_matching(
            user_trap_assembly, r"^add\s+sp,\s*sp,\s*#0x20$"
        ),
        "user_restore_ttbr0_writes": count_matching(
            user_trap_assembly, r"^msr\s+ttbr0_el1\b"
        ),
        "user_restore_vbar_writes": count_matching(
            user_trap_assembly, r"^msr\s+vbar_el1\b"
        ),
        "user_restore_ttbr0_before_vbar": int(
            ttbr0_write is not None
            and vbar_write is not None
            and ttbr0_write < vbar_write
        ),
        "pre_restore_vbar_writes": count_matching(user_return, r"^msr\s+vbar_el1\b"),
        "kernel_irq_elr_system": count_matching(
            kernel_irq, r"^(?:mrs\s+\w+,\s*elr_el1|msr\s+elr_el1\b)"
        ),
        "kernel_irq_wfi_label_addresses": sum(
            "__local_irq_wait_wfi" in line for line in kernel_irq
        ),
        "bootstrap_wait_wfi": count_matching(bootstrap_wait, r"^wfi$"),
        "bootstrap_wait_daif_writes": count_matching(
            bootstrap_wait, r"^msr\s+daif(?:clr|set)\b"
        ),
        "switch_q_store_instructions": count_matching(
            context_switch, r"^stp\s+q(?:[0-9]|[12][0-9]|3[01]),\s*q"
        ),
        "switch_q_load_instructions": count_matching(
            context_switch, r"^ldp\s+q(?:[0-9]|[12][0-9]|3[01]),\s*q"
        ),
        "switch_saved_q_registers": len(saved),
        "switch_restored_q_registers": len(restored),
        "switch_saved_q_register_set": int(sorted(saved) == list(range(32))),
        "switch_restored_q_register_set": int(sorted(restored) == list(range(32))),
        "switch_fp_system": count_matching(context_switch, r"\b(?:fpcr|fpsr)\b"),
        "switch_cpacr_writes": count_matching(context_switch, r"^msr\s+cpacr_el1\b"),
        "switch_isb": count_matching(context_switch, r"^isb$"),
    }
    expected = {
        "ordinary_fp_load_store": 0,
        "ordinary_q_instructions": 0,
        "ordinary_fp_system": 0,
        "user_contextidr_reads": 0,
        "user_contextidr_writes": 0,
        "user_context_stack_address_adds": 1,
        "user_context_metadata_loads": 0,
        "user_context_metadata_stores": 0,
        "user_tpidrro_reads": 0,
        "user_tpidrro_zero_writes": 1,
        "user_tpidr_el0_writes": 1,
        "user_scratch_stack_reserves": 2,
        "user_scratch_stack_releases": 1,
        "user_restore_ttbr0_writes": 1,
        "user_restore_vbar_writes": 1,
        "user_restore_ttbr0_before_vbar": 1,
        "pre_restore_vbar_writes": 0,
        "kernel_irq_elr_system": 2,
        "kernel_irq_wfi_label_addresses": 2,
        "bootstrap_wait_wfi": 1,
        "bootstrap_wait_daif_writes": 2,
        "switch_q_store_instructions": 16,
        "switch_q_load_instructions": 16,
        "switch_saved_q_registers": 32,
        "switch_restored_q_registers": 32,
        "switch_saved_q_register_set": 1,
        "switch_restored_q_register_set": 1,
        "switch_fp_system": 4,
        "switch_cpacr_writes": 2,
        "switch_isb": 2,
    }
    helper_lines = {
        "signal_capture": (signal_capture, "stp"),
        "signal_restore": (signal_restore, "ldp"),
        "clone_capture": (clone_capture, "stp"),
        "exec_reset": (exec_reset, "ldp"),
    }
    for name, (lines, operation) in helper_lines.items():
        registers = helper_registers[name]
        measured[f"{name}_q_instructions"] = count_matching(lines, rf"^{operation}\s+q")
        measured[f"{name}_q_registers"] = len(registers)
        measured[f"{name}_q_register_set"] = int(sorted(registers) == list(range(32)))
        measured[f"{name}_fp_system"] = count_matching(lines, r"\b(?:fpcr|fpsr)\b")
        measured[f"{name}_cpacr_writes"] = count_matching(
            lines, r"^msr\s+cpacr_el1\b"
        )
        measured[f"{name}_isb"] = count_matching(lines, r"^isb$")
        expected[f"{name}_q_instructions"] = 16
        expected[f"{name}_q_registers"] = 32
        expected[f"{name}_q_register_set"] = 1
        expected[f"{name}_fp_system"] = 2
        expected[f"{name}_cpacr_writes"] = 2
        expected[f"{name}_isb"] = 2
    return measured, expected


def inspect_target(
    target: BuildTarget, tool: str, elf: Path
) -> tuple[str, dict[str, int], dict[str, int]]:
    table = symbols(tool, elf)
    if target.arch == "riscv64":
        trap = instructions(tool, elf, table, "__alltraps") + instructions(
            tool, elf, table, "__restore"
        )
        user_return = instructions(
            tool, elf, table, "kernel::arch::riscv64::trap::return_to_user"
        )
        measured, expected = riscv_measurements(trap, user_return)
        return "release integer-only RISC-V user-trap path", measured, expected

    ordinary = instructions(tool, elf, table, "__aarch64_vectors")
    ordinary += instructions(tool, elf, table, "__liteos_kernel_trap")
    ordinary += instructions(tool, elf, table, "__liteos_user_trap")
    user_trap_assembly = []
    for symbol in (
        "__aarch64_user_sync",
        "__aarch64_user_irq",
        "__aarch64_user_common",
        "__aarch64_restore",
    ):
        user_trap_assembly += instructions(tool, elf, table, symbol)
    kernel_irq = instructions(tool, elf, table, "__aarch64_kernel_irq")
    bootstrap_wait = []
    for symbol in (
        "__wait_with_local_irq_masked",
        "__local_irq_wait_wfi",
        "__local_irq_wait_wfi_resume",
    ):
        bootstrap_wait += instructions(tool, elf, table, symbol)
    user_return = instructions(tool, elf, table, "kernel::trap::trap_return")
    context_switch = instructions(tool, elf, table, "__switch")
    signal_capture = instructions(tool, elf, table, "__aarch64_signal_fp_capture")
    signal_restore = instructions(tool, elf, table, "__aarch64_signal_fp_restore")
    clone_capture = instructions(tool, elf, table, "__aarch64_clone_fp_capture")
    exec_reset = instructions(tool, elf, table, "__aarch64_exec_fp_reset")
    measured, expected = aarch64_measurements(
        ordinary,
        user_trap_assembly,
        kernel_irq,
        bootstrap_wait,
        user_return,
        context_switch,
        signal_capture,
        signal_restore,
        clone_capture,
        exec_reset,
    )
    return "release AArch64 trap/context-switch path", measured, expected


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--build", action="store_true", help="build the release target first")
    arguments = parser.parse_args()
    target = target_from_environment()
    elf = ROOT / "target" / target.kernel_triple / "release/kernel"
    if arguments.build:
        subprocess.run(
            ["cargo", "build", "--release", "--target", target.kernel_triple],
            cwd=ROOT / "kernel",
            check=True,
        )
    if not elf.exists():
        parser.error(f"missing release kernel: {elf}")

    label, measured, expected = inspect_target(target, llvm_objdump(), elf)
    print(f"{label}:")
    failed = False
    for metric, value in measured.items():
        required = expected[metric]
        if target.arch == "riscv64":
            accepted = value <= required
            expectation = f"limit {required}"
        else:
            accepted = value == required
            expectation = f"required {required}"
        verdict = "ok" if accepted else "RED"
        print(f"- {metric}: {value} ({expectation}) [{verdict}]")
        failed |= not accepted
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, StopIteration, subprocess.CalledProcessError) as error:
        print(f"trap-cost gate failed to inspect release ELF: {error}", file=sys.stderr)
        sys.exit(2)
