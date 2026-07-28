#!/usr/bin/env python3
"""Enforce the LiteUI 60Hz frame-timing budget on the guest vblank clock.

This is the sole explicit exception to the repo's rejection of wall-clock timing
gates (docs/development/build-and-verify.md): it does NOT measure the Python host
clock. The compositor stamps every real page flip with the guest KERNEL MONOTONIC
vblank time (DRM page-flip seconds/microseconds via get_time_ns() over the DTB
timebase) and the device presentation sequence, and emits a `compositor:
frame-stats` marker per window. This gate parses that marker, driving a synthetic
virtio-input workload so an otherwise-idle desktop produces a measurable stream.

Gate policy (microseconds), per build-and-verify.md's "wide-but-real absolute
ceilings" rule: the contract numbers (lite-ui.md:87-89, p95 16.67ms / p99 33.3ms)
are printed as context, but the RED thresholds are widened so host scheduling
jitter under QEMU+HVF does not flap the gate. `dropped` counts device vblank
SEQUENCE gaps — independent of any host clock — so it is gated strictly at 0 as
the sharpest real signal. AArch64+HVF only; RISC-V TCG does not carry the 60Hz
gate (lite-ui.md:91).
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from build_cache import publish_runtime_gate, runtime_gate_hit, runtime_gate_payload
from build_target import BuildTarget, target_from_environment
from qemu_gate import measure_frame_timing

ROOT = Path(__file__).resolve().parent.parent

# Must match `WINDOW` in user/compositor/src/frame_stats.rs: the compositor emits
# one frame-stats marker per this many frames, so a full window is the minimum
# real sample. Kept in sync by hand (two languages, one constant).
WINDOW = 512

# Contract numbers (context only; never gated directly, never recorded in docs).
CONTRACT_P95_US = 16_670  # 16.67 ms
CONTRACT_P99_US = 33_300  # 33.3 ms
# Wide-but-real RED ceilings. p95 gets one 60Hz frame of slack over contract;
# above it the compositor genuinely missed frames rather than jittered. p99
# catches sustained multi-frame stalls without tripping on single outliers.
LIMIT_P95_US = 33_300
LIMIT_P99_US = 50_000
MAX_DROPPED = 0  # any vblank sequence gap is a real dropped frame.
# Bump to force a re-run after tightening thresholds or the marker format.
RECIPE_VERSION = 2


def default_image(target: BuildTarget) -> Path:
    """Returns the target-isolated read-only rootfs baseline."""
    return ROOT / "target" / "rootfs" / f"{target.arch}.img"


def gate_inputs(image: Path, target: BuildTarget) -> tuple[Path, ...]:
    """Cache inputs = only artifacts that can change measured frame timing.

    The rootfs image is the ground truth of what boots (the compositor, lite-ui
    and terminal-session binaries are baked into it by build-rootfs), so hashing
    it captures any userland change. The kernel boot artifact and the two gate
    scripts complete the key.
    """
    return (
        image,
        ROOT / target.kernel_boot_artifact(),
        ROOT / "scripts" / "qemu_gate.py",
        Path(__file__).resolve(),
    )


def report(measured: dict[str, int]) -> int:
    """Prints the per-metric verdict and returns 1 if any threshold is red."""
    print("AArch64+HVF 60Hz frame-timing (guest vblank clock):")
    print(f"  contract p95<= {CONTRACT_P95_US}us p99<= {CONTRACT_P99_US}us (context)")
    limits = {
        "p95_us": LIMIT_P95_US,
        "p99_us": LIMIT_P99_US,
        "dropped": MAX_DROPPED,
    }
    failed = False
    for metric, limit in limits.items():
        value = measured[metric]
        accepted = value <= limit
        verdict = "ok" if accepted else "RED"
        print(f"- {metric}: {value} (limit {limit}) [{verdict}]")
        failed |= not accepted
    # A short window means the workload never produced a real steady stream; a
    # silent no-frame run must not pass as green.
    frames = measured["frames"]
    if frames < WINDOW:
        print(f"- frames: {frames} (need >= {WINDOW}) [RED, too few samples]")
        failed = True
    else:
        print(f"- frames: {frames} (>= {WINDOW}) [ok]")
    return 1 if failed else 0


def main() -> int:
    target = target_from_environment()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--image",
        type=Path,
        default=default_image(target),
        help="read-only baseline rootfs; the gate never modifies it",
    )
    args = parser.parse_args()
    if target.arch != "aarch64":
        print(f"frame-timing gate skipped: {target.arch} does not carry the 60Hz gate")
        return 0
    image = args.image.resolve()
    try:
        if not image.is_file():
            raise RuntimeError(f"rootfs image is missing: {image}")
        stamp = ROOT / "target" / "verify-gates" / "frame-timing-aarch64.json"
        payload = runtime_gate_payload(
            "frame-timing",
            RECIPE_VERSION,
            gate_inputs(image, target),
        )
        if runtime_gate_hit(stamp, payload, (image,)):
            print("frame-timing verification cache hit")
            return 0
        measured = measure_frame_timing(image)
        code = report(measured)
        if code != 0:
            return code
        publish_runtime_gate(stamp, payload)
    except RuntimeError as error:
        print(f"frame-timing verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
