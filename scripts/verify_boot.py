#!/usr/bin/env python3
"""Run deterministic non-test QEMU cold boots against several DTB hart sets."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

from qemu_gate import boot as boot_image

ROOT = Path(__file__).resolve().parent.parent
SMP_CONFIGURATIONS = (1, 3, 8)


def create_fresh_filesystem() -> None:
    subprocess.run(
        [sys.executable, "create_fs.py", "create"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )


def boot(smp: int) -> None:
    create_fresh_filesystem()
    expected_mask = (1 << smp) - 1
    markers = (
        f"dynamic hart topology initialized: count={smp}, mask={expected_mask:#x}",
        f"all DTB harts online: count={smp}, mask={expected_mask:#x}",
        "LiteOS init",
        "vma ok",
        "process ok",
        "thread futex ok",
        "signal ok",
        "ext2 rw ok",
    )
    boot_image(ROOT / "fs.img", smp, markers)


def main() -> int:
    try:
        for smp in SMP_CONFIGURATIONS:
            boot(smp)
            print(f"QEMU -smp {smp} boot verification passed")
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"boot verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
