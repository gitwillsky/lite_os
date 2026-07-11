#!/usr/bin/env python3
"""Run deterministic non-test QEMU cold boots against several DTB hart sets."""

from __future__ import annotations

import os
import re
import select
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ANSI = re.compile(r"\x1b\[[0-9;]*m")
BOOT_TIMEOUT_SECONDS = 30
SMP_CONFIGURATIONS = (1, 3, 8)


def terminate(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=3)


def create_fresh_filesystem() -> None:
    subprocess.run(
        [sys.executable, "create_fs.py", "create"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )


def boot(qemu: str, smp: int) -> None:
    create_fresh_filesystem()
    expected_mask = (1 << smp) - 1
    markers = (
        f"dynamic hart topology initialized: count={smp}, mask={expected_mask:#x}",
        f"all DTB harts online: count={smp}, mask={expected_mask:#x}",
        "LiteOS init",
        "vma ok",
        "process ok",
        "thread futex ok",
        "ext2 rw ok",
    )
    command = [
        qemu,
        "-machine",
        "virt",
        "-nographic",
        "-smp",
        str(smp),
        "-rtc",
        "base=utc",
        "-bios",
        "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
        "-kernel",
        "target/riscv64gc-unknown-none-elf/debug/kernel",
        "-drive",
        "file=fs.img,if=none,format=raw,id=x0",
        "-device",
        "virtio-blk-device,drive=x0",
    ]
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    assert process.stdout is not None
    output = bytearray()
    deadline = time.monotonic() + BOOT_TIMEOUT_SECONDS
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([process.stdout], [], [], 0.25)
            if ready:
                chunk = os.read(process.stdout.fileno(), 16 * 1024)
                if not chunk:
                    break
                output.extend(chunk)
                text = ANSI.sub("", output.decode(errors="replace"))
                if all(marker in text for marker in markers):
                    if "panicked at" in text or "[ERROR]" in text:
                        raise RuntimeError(f"QEMU -smp {smp} reached a fatal/error path")
                    return
            if process.poll() is not None:
                break
    finally:
        terminate(process)

    text = ANSI.sub("", output.decode(errors="replace"))
    missing = [marker for marker in markers if marker not in text]
    tail = "\n".join(text.splitlines()[-40:])
    raise RuntimeError(
        f"QEMU -smp {smp} boot gate failed; missing={missing!r}\n--- output tail ---\n{tail}"
    )


def main() -> int:
    qemu = shutil.which("qemu-system-riscv64")
    if not qemu:
        print("boot verification failed: qemu-system-riscv64 is required", file=sys.stderr)
        return 1
    try:
        for smp in SMP_CONFIGURATIONS:
            boot(qemu, smp)
            print(f"QEMU -smp {smp} boot verification passed")
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"boot verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
