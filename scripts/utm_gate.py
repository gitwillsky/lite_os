#!/usr/bin/env python3
"""Product VirGL runtime gates executed by the pinned UTM backend."""

from __future__ import annotations

import os
import select
import time
from pathlib import Path

from qemu_gate import ANSI
from utm_runtime import start_gate


def _tail(text: str, lines: int = 40) -> str:
    return "\n".join(text.splitlines()[-lines:])


def boot_graphics(
    image: Path,
    kernel: Path,
    markers: tuple[str, ...],
    timeout_seconds: float = 180.0,
    kernel_arguments: str | None = None,
    interactions: tuple[tuple[str, bytes], ...] = (),
) -> str:
    """Cold-boot the sole UTM VirGL path until all markers are visible; return the serial text.

    `kernel_arguments` is the guest kernel command line.
    Each `(marker, data)` in `interactions` is typed into the serial shell once, the first time
    `marker` appears in the output, in order.
    """
    runtime = start_gate(kernel=kernel, rootfs=image, kernel_arguments=kernel_arguments)
    pending = list(interactions)
    output = bytearray()
    deadline = time.monotonic() + timeout_seconds
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([runtime.stdout], [], [], 0.25)
            if not ready:
                continue
            chunk = os.read(runtime.stdout.fileno(), 16 * 1024)
            if not chunk:
                break
            output.extend(chunk)
            text = ANSI.sub("", output.decode(errors="replace"))
            if "panicked at" in text or "[ERROR]" in text:
                raise RuntimeError(
                    "UTM graphics gate reached a fatal path"
                    f"\n--- output tail ---\n{_tail(text)}"
                )
            while pending and pending[0][0] in text:
                runtime.write(pending.pop(0)[1])
            if not pending and all(marker in text for marker in markers):
                return text
    finally:
        runtime.close()
    text = ANSI.sub("", output.decode(errors="replace"))
    missing = [marker for marker in markers if marker not in text]
    raise RuntimeError(
        f"UTM graphics boot gate failed; missing={missing!r}"
        f"\n--- output tail ---\n{_tail(text)}"
    )
