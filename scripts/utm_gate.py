#!/usr/bin/env python3
"""Product VirGL runtime gates executed by the pinned UTM backend."""

from __future__ import annotations

import os
import select
import threading
import time
from pathlib import Path

from qemu_gate import (
    ANSI,
    FRAME_STATS_RE,
    QmpClient,
    start_frame_workload,
)
from utm_runtime import GateRuntime, start_gate


def _tail(text: str, lines: int = 40) -> str:
    return "\n".join(text.splitlines()[-lines:])


def boot_graphics(
    image: Path,
    kernel: Path,
    markers: tuple[str, ...],
    timeout_seconds: float = 180.0,
) -> None:
    """Cold-boot the sole UTM VirGL path until all product markers are visible."""
    runtime = start_gate(kernel=kernel, rootfs=image)
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
            if all(marker in text for marker in markers):
                return
    finally:
        runtime.close()
    text = ANSI.sub("", output.decode(errors="replace"))
    missing = [marker for marker in markers if marker not in text]
    raise RuntimeError(
        f"UTM graphics boot gate failed; missing={missing!r}"
        f"\n--- output tail ---\n{_tail(text)}"
    )


def measure_frame_timing(
    image: Path,
    kernel: Path,
    settle_s: float = 90.0,
    timeout_seconds: int = 180,
) -> dict[str, int]:
    """Drive and measure the sole UTM VirGL compositor path through QMP."""
    runtime = start_gate(kernel=kernel, rootfs=image, qmp=True)
    output = bytearray()
    output_lock = threading.Lock()
    stop_reading = threading.Event()

    def reader() -> None:
        while not stop_reading.is_set():
            ready, _, _ = select.select([runtime.stdout], [], [], 0.1)
            if not ready:
                continue
            chunk = os.read(runtime.stdout.fileno(), 16 * 1024)
            if not chunk:
                return
            with output_lock:
                output.extend(chunk)

    def current_text() -> str:
        with output_lock:
            return ANSI.sub("", bytes(output).decode(errors="replace"))

    def parse_windows(text: str) -> list[dict[str, int]]:
        found: list[dict[str, int]] = []
        for match in FRAME_STATS_RE.finditer(text):
            found.append(
                {
                    "window": int(match.group(1)),
                    "frames": int(match.group(2)),
                    "dropped": int(match.group(3)),
                    "p50_us": int(match.group(4)),
                    "p95_us": int(match.group(5)),
                    "p99_us": int(match.group(6)),
                }
            )
        return found

    reader_thread = threading.Thread(target=reader, daemon=True)
    reader_thread.start()
    qmp: QmpClient | None = None
    windows: list[dict[str, int]] = []
    phase_windows: dict[str, int] = {}
    second_window_opened = False
    deadline = time.monotonic() + timeout_seconds
    try:
        boot_markers = (
            "init started: BusyBox v1.37.0",
            "compositor: GPU mode",
            "compositor: desktop connected",
            "compositor: desktop first scene presented",
            "lite-ui: desktop ready",
        )
        while time.monotonic() < deadline:
            text = current_text()
            if "panicked at" in text or "[ERROR]" in text:
                raise RuntimeError(
                    "frame-timing guest reached a fatal path"
                    f"\n--- output tail ---\n{_tail(text)}"
                )
            if all(marker in text for marker in boot_markers):
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("frame-timing gate timed out before desktop was ready")

        if runtime.qmp_socket is None:
            raise RuntimeError("UTM frame-timing gate has no QMP endpoint")
        qmp = QmpClient(runtime.qmp_socket)

        def wait_for(markers: tuple[str, ...], phase: str) -> None:
            phase_deadline = min(deadline, time.monotonic() + 20.0)
            while time.monotonic() < phase_deadline:
                text = current_text()
                if "panicked at" in text or "[ERROR]" in text:
                    raise RuntimeError(
                        f"frame-timing guest failed during {phase}"
                        f"\n--- output tail ---\n{_tail(text)}"
                    )
                if all(marker in text for marker in markers):
                    return
                time.sleep(0.1)
            missing = [marker for marker in markers if marker not in current_text()]
            raise RuntimeError(f"frame-timing gate missed {phase} markers: {missing!r}")

        wait_for(
            (
                "lite-ui: desktop startup motion settled",
                "compositor: app 1 connected",
                "lite-ui: terminal session ready",
                "lite-ui: app terminal ready",
                "terminal-session: shell spawned",
                "compositor: app 2 connected",
                "lite-ui: app file-manager ready",
            ),
            "Aurora pinned apps",
        )
        second_window_opened = True
        workload_deadline = min(deadline, time.monotonic() + settle_s)

        def phase_finished(phase: str) -> None:
            text = current_text()
            phase_windows[phase] = len(parse_windows(text))

        start_frame_workload(
            qmp,
            workload_deadline - time.monotonic(),
            stop_reading,
            phase_finished,
        )
        windows = parse_windows(current_text())
        if not windows:
            time.sleep(0.5)
            windows = parse_windows(current_text())
        text_running = current_text()
        if "panicked at" in text_running or "[ERROR]" in text_running:
            raise RuntimeError(
                "frame-timing guest reached a fatal path"
                f"\n--- output tail ---\n{_tail(text_running)}"
            )
        if (
            "move grab rejected" in text_running
            or "desktop move underlay buffer unavailable" in text_running
            or "compositor: desktop disconnected" in text_running
            or text_running.count("lite-ui: desktop ready") != 1
        ):
            raise RuntimeError(
                "frame-timing workload left the authorized interaction lifecycle"
                f"\n--- output tail ---\n{_tail(text_running)}"
            )
        previous = 0
        for phase in ("resize", "scroll", "move"):
            current = phase_windows.get(phase, 0)
            print(f"frame-timing phase={phase} windows={windows[previous:current]!r}")
            if current <= previous:
                raise RuntimeError(
                    f"frame-timing {phase} phase did not complete a frame-stats window; "
                    f"phase_windows={phase_windows!r}"
                    f"\n--- output tail ---\n{_tail(text_running)}"
                )
            previous = current
    finally:
        stop_reading.set()
        reader_thread.join(timeout=2)
        if qmp is not None:
            qmp.close()
        text_final = current_text()
        runtime.close()
    if len(windows) < 2:
        raise RuntimeError(
            "frame-timing gate needs one warmup and one steady frame-stats window "
            f"(collected={len(windows)}, second_window_opened={second_window_opened})"
            f"\n--- output tail ---\n{_tail(text_final)}"
        )
    steady = windows[1:]
    return max(steady, key=lambda window: (window["dropped"], window["p99_us"], window["p95_us"]))
