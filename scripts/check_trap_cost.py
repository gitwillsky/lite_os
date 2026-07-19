#!/usr/bin/env python3
"""Gate the release RISC-V integer-only user-trap instruction path."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ELF = ROOT / "target/riscv64gc-unknown-none-elf/release/kernel"
OBJDUMP = "rust-objdump"


@dataclass(frozen=True)
class Symbol:
    address: int
    size: int
    name: str


def run(command: list[str]) -> str:
    return subprocess.run(
        command,
        cwd=ROOT,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout


def symbols() -> list[Symbol]:
    output = run([OBJDUMP, "-t", "-C", str(ELF)])
    parsed = []
    pattern = re.compile(
        r"^([0-9a-f]+)\s+\S*\s*(?:F\s+)?\.text\s+([0-9a-f]+)\s+(.+)$"
    )
    for line in output.splitlines():
        match = pattern.match(line)
        if match:
            parsed.append(
                Symbol(int(match.group(1), 16), int(match.group(2), 16), match.group(3))
            )
    return sorted(parsed, key=lambda symbol: symbol.address)


def find_symbol(table: list[Symbol], name: str) -> Symbol:
    return next(symbol for symbol in table if symbol.name == name)


def instructions(table: list[Symbol], name: str) -> list[str]:
    symbol = find_symbol(table, name)
    stop = symbol.address + symbol.size
    if symbol.size == 0:
        stop = next(
            candidate.address
            for candidate in table
            if candidate.address > symbol.address
        )
    output = run(
        [
            OBJDUMP,
            "-d",
            "-C",
            f"--start-address=0x{symbol.address:x}",
            f"--stop-address=0x{stop:x}",
            str(ELF),
        ]
    )
    decoded = []
    for line in output.splitlines():
        match = re.match(r"^\s*[0-9a-f]+:\s+[0-9a-f]+\s+(.+)$", line)
        if match:
            decoded.append(" ".join(match.group(1).split()))
    return decoded


def count_matching(lines: list[str], pattern: str) -> int:
    expression = re.compile(pattern)
    return sum(expression.search(line) is not None for line in lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--build", action="store_true", help="build the release target first")
    arguments = parser.parse_args()
    if arguments.build:
        subprocess.run(
            ["cargo", "build", "--release"],
            cwd=ROOT / "kernel",
            check=True,
        )
    if not ELF.exists():
        parser.error(f"missing release kernel: {ELF}")

    table = symbols()
    trap = instructions(table, "__alltraps") + instructions(table, "__restore")
    user_return = instructions(table, "kernel::arch::riscv64::trap::return_to_user")
    measured = {
        "satp_writes": count_matching(trap, r"\bcsrw\s+satp\b"),
        "full_sfence_vma": count_matching(trap, r"^sfence\.vma$"),
        "fp_stores": count_matching(trap, r"^fs[wdq]\b"),
        "fp_loads": count_matching(trap, r"^fl[wdq]\b"),
        "fence_i": count_matching(user_return, r"^fence\.i$"),
    }
    limits = {
        "satp_writes": 2,
        "full_sfence_vma": 0,
        "fp_stores": 0,
        "fp_loads": 0,
        "fence_i": 0,
    }
    print("release integer-only user-trap path:")
    failed = False
    for metric, value in measured.items():
        limit = limits[metric]
        verdict = "ok" if value <= limit else "RED"
        print(f"- {metric}: {value} (limit {limit}) [{verdict}]")
        failed |= value > limit
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (StopIteration, subprocess.CalledProcessError) as error:
        print(f"trap-cost gate failed to inspect release ELF: {error}", file=sys.stderr)
        sys.exit(2)
