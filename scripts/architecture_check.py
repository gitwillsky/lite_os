#!/usr/bin/env python3
"""LiteOS 全仓 module、ABI、interface 与安全能力围栏。"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCE_ROOTS = (ROOT / "kernel/src", ROOT / "bootloader/src")
INTERFACE_BASELINE = ROOT / "docs/architecture-interface.txt"


def rust_files(root: Path) -> list[Path]:
    return sorted(root.rglob("*.rs"))


def relative(path: Path) -> str:
    return path.relative_to(ROOT).as_posix()


def fail(errors: list[str], message: str) -> None:
    errors.append(message)


def crate_dependencies(text: str) -> set[str]:
    """Return top-level modules referenced through direct or braced crate paths."""
    dependencies = set(re.findall(r"\bcrate\s*::\s*([A-Za-z_][A-Za-z0-9_]*)", text))
    for match in re.finditer(r"\buse\s+crate\s*::\s*\{", text):
        cursor = match.end()
        depth = 0
        entry_start = True
        while cursor < len(text):
            char = text[cursor]
            if depth == 0 and char == "}":
                break
            if depth == 0 and entry_start:
                name = re.match(r"\s*([A-Za-z_][A-Za-z0-9_]*)", text[cursor:])
                if name:
                    dependencies.add(name.group(1))
                    cursor += name.end()
                    entry_start = False
                    continue
            if char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
            elif char == "," and depth == 0:
                entry_start = True
            cursor += 1
    return dependencies


def check_forbidden_dependencies(errors: list[str]) -> None:
    rules = {
        "kernel/src/arch": ("task", "fs", "drivers", "syscall", "trap"),
        "kernel/src/sync": ("task", "fs", "drivers", "syscall", "trap"),
        "kernel/src/memory": ("task", "fs", "drivers", "syscall", "trap"),
        "kernel/src/drivers": ("task", "fs", "syscall", "trap"),
        "kernel/src/fs": ("task", "syscall", "trap"),
        "kernel/src/task": ("drivers", "syscall", "trap"),
        "kernel/src/syscall": ("drivers", "arch", "trap"),
    }
    for directory, forbidden in rules.items():
        for path in rust_files(ROOT / directory):
            text = path.read_text()
            dependencies = crate_dependencies(text)
            for target in sorted(set(forbidden) & dependencies):
                fail(errors, f"{relative(path)}: forbidden dependency on crate::{target}")

    for path in rust_files(ROOT / "kernel/src/fs"):
        text = path.read_text()
        for match in re.finditer(r"\bcrate\s*::\s*drivers\s*::\s*([A-Za-z_][A-Za-z0-9_]*)", text):
            if match.group(1) != "block":
                fail(errors, f"{relative(path)}: filesystem may depend only on drivers::block seam")

    for path in rust_files(ROOT / "kernel/src/syscall"):
        text = path.read_text()
        if "fs::ext2" in text or "memory::page_table" in text or "task::scheduler" in text:
            fail(errors, f"{relative(path)}: syscall bypasses a domain interface")

    for path in rust_files(ROOT / "bootloader/src"):
        if re.search(r"\b(kernel|user)::", path.read_text()):
            fail(errors, f"{relative(path)}: bootloader must remain an independent M-mode domain")
    for path in rust_files(ROOT / "user/src"):
        if re.search(r"\b(kernel|bootloader)::", path.read_text()):
            fail(errors, f"{relative(path)}: user may depend only on syscall-abi and its runtime")


def check_source_patterns(errors: list[str]) -> None:
    banned = {
        r"\bMAX_CORES\b": "fixed CPU capacity",
        r"\bstart_all_cores\b": "firmware-driven secondary startup",
        r"\bSTDOUT_FILENO\b": "stdout syscall side path",
        r"^\s*(?:unsafe\s+)?static\s+mut\b": "static mut global state",
        r"\b0\s*\.\.\s*8\b": "dense eight-hart iteration",
        r"read[_ -]?only ext2|只读 ext2": "read-only filesystem dual track",
        r"\b(?:todo|unimplemented)!\s*\(": "unfinished executable path",
    }
    for root in SOURCE_ROOTS:
        for path in rust_files(root):
            text = path.read_text()
            for pattern, label in banned.items():
                if re.search(pattern, text, re.IGNORECASE | re.MULTILINE):
                    fail(errors, f"{relative(path)}: banned pattern reintroduces {label}")

    garbage_names = {"common", "utils", "helpers", "misc", "manager", "base", "shared", "core"}
    for root in (ROOT / "kernel/src", ROOT / "bootloader/src", ROOT / "user/src"):
        for path in root.rglob("*"):
            if path.is_dir() and path.name in garbage_names:
                fail(errors, f"{relative(path)}: directory name has no domain meaning")

    for root in SOURCE_ROOTS:
        for path in rust_files(root):
            for number, line in enumerate(path.read_text().splitlines(), 1):
                if re.match(r"\s*pub\s+(?!\()", line):
                    fail(errors, f"{relative(path)}:{number}: binary-crate implementation must use scoped visibility")


def interface_surface() -> list[str]:
    surface: list[str] = []
    pattern = re.compile(r"^\s*pub\(crate\)\s+(.+)$")
    for root in SOURCE_ROOTS:
        for path in rust_files(root):
            lines = path.read_text().splitlines()
            for index, line in enumerate(lines):
                match = pattern.match(line)
                if match:
                    declaration = match.group(1).strip()
                    # Function and tuple-struct declarations may span lines. Capture through the
                    # signature terminator so parameter/type changes cannot bypass the contract.
                    if ("fn " in declaration or declaration.startswith("struct ")) and not re.search(
                        r"[;{]", declaration
                    ):
                        cursor = index + 1
                        while cursor < len(lines):
                            declaration += " " + lines[cursor].strip()
                            if re.search(r"[;{]", lines[cursor]):
                                break
                            cursor += 1
                    declaration = re.sub(r"\s+", " ", declaration)
                    surface.append(f"{relative(path)} :: {declaration}")
    return sorted(surface)


def check_interface(errors: list[str], write: bool) -> None:
    current = interface_surface()
    if write:
        INTERFACE_BASELINE.write_text(
            "# Generated by scripts/architecture_check.py --write-interface\n"
            "# Any change is an architecture interface change and must be reviewed.\n"
            + "\n".join(current)
            + "\n"
        )
        return
    expected = [
        line
        for line in INTERFACE_BASELINE.read_text().splitlines()
        if line and not line.startswith("#")
    ]
    if current != expected:
        missing = sorted(set(expected) - set(current))
        added = sorted(set(current) - set(expected))
        for item in missing:
            fail(errors, f"public interface removed or changed without contract update: {item}")
        for item in added:
            fail(errors, f"public interface expanded without contract update: {item}")


def check_abi(errors: list[str]) -> None:
    abi = (ROOT / "syscall-abi/src/lib.rs").read_text()
    dispatch = (ROOT / "kernel/src/syscall/mod.rs").read_text()
    constants = set(re.findall(r"pub const (SYSCALL_[A-Z0-9_]+):", abi))
    routed = set(re.findall(r"\b(SYSCALL_[A-Z0-9_]+)\b", dispatch))
    for name in sorted(constants - routed):
        fail(errors, f"syscall ABI constant is not dispatched: {name}")
    for name in sorted(routed - constants):
        fail(errors, f"dispatcher uses a syscall absent from syscall-abi: {name}")
    if re.search(r"^\s*\d+\s*=>", dispatch, re.MULTILINE):
        fail(errors, "kernel/src/syscall/mod.rs: raw numeric syscall dispatch is forbidden")


def check_global_owners(errors: list[str]) -> None:
    pattern = re.compile(r"^\s*(?:pub\(crate\)\s+)?static(?:\s+ref)?\s+[A-Z_][A-Z0-9_]*")
    for root in SOURCE_ROOTS:
        for path in rust_files(root):
            lines = path.read_text().splitlines()
            for index, line in enumerate(lines):
                if not pattern.match(line):
                    continue
                context = "\n".join(lines[max(0, index - 4):index])
                if "OWNER:" not in context:
                    fail(errors, f"{relative(path)}:{index + 1}: global state lacks an OWNER declaration")


def check_unsafe_proofs(errors: list[str]) -> None:
    unsafe_pattern = re.compile(r"\bunsafe\s*(?:\{|impl\b|fn\b)")
    for root in (*SOURCE_ROOTS, ROOT / "user/src"):
        for path in rust_files(root):
            lines = path.read_text().splitlines()
            for index, line in enumerate(lines):
                if not unsafe_pattern.search(line):
                    continue
                context = "\n".join(lines[max(0, index - 6):index + 1])
                if "SAFETY:" not in context:
                    fail(errors, f"{relative(path)}:{index + 1}: unsafe operation lacks a local SAFETY proof")


def check_documentation(errors: list[str]) -> None:
    count = len(re.findall(r"pub const SYSCALL_", (ROOT / "syscall-abi/src/lib.rs").read_text()))
    readme = (ROOT / "README.md").read_text()
    if f"{count} 个 Linux/riscv64 syscall" not in readme:
        fail(errors, f"README syscall count must match syscall-abi ({count})")
    agents = (ROOT / "AGENTS.md").read_text()
    forbidden_snapshots = ("个 Linux/riscv64 syscall", "同步读写 ext2", "nightly-", "三个组件")
    for phrase in forbidden_snapshots:
        if phrase in agents:
            fail(errors, f"AGENTS.md duplicates discoverable project state: {phrase}")
    architecture = (ROOT / "docs/architecture.md").read_text()
    stale_current_claims = (
        "startup stack、processor slot",
        "- fd/OFD、普通文件 I/O",
        "- writable filesystem",
        "**fd/OFD 文件竖切**",
    )
    for phrase in stale_current_claims:
        if phrase in architecture:
            fail(errors, f"docs/architecture.md retains superseded current-state claim: {phrase}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--write-interface", action="store_true")
    args = parser.parse_args()
    errors: list[str] = []
    check_forbidden_dependencies(errors)
    check_source_patterns(errors)
    check_abi(errors)
    check_global_owners(errors)
    check_unsafe_proofs(errors)
    check_documentation(errors)
    check_interface(errors, args.write_interface)
    if errors:
        print("architecture fence failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print("architecture fence passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
