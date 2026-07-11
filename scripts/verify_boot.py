#!/usr/bin/env python3
"""Run deterministic non-test QEMU cold boots against several DTB hart sets."""

from __future__ import annotations

import sys
from pathlib import Path

from qemu_gate import boot as boot_image

ROOT = Path(__file__).resolve().parent.parent
SMP_CONFIGURATIONS = (1, 3, 8)


def boot(smp: int) -> None:
    expected_mask = (1 << smp) - 1
    markers = (
        f"dynamic hart topology initialized: count={smp}, mask={expected_mask:#x}",
        f"all DTB harts online: count={smp}, mask={expected_mask:#x}",
        "init started: BusyBox v1.37.0",
    )
    boot_image(ROOT / "fs.img", smp, markers)


def main() -> int:
    try:
        for smp in SMP_CONFIGURATIONS:
            boot(smp)
            print(f"QEMU -smp {smp} boot verification passed")
    except RuntimeError as error:
        print(f"boot verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
