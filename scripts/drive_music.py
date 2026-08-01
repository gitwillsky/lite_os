#!/usr/bin/env python3
"""Acceptance driver for the online music player: boots the desktop, opens the
music app, runs an online search, and screenshots search + now-playing to prove
the lite:net TLS/DNS path and streaming playback work end to end."""
import os
import select
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from qemu_gate import ANSI, QmpClient, _qemu_command, terminate  # noqa: E402

IMAGE = ROOT / "fs-aarch64.img"
OUT = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("/tmp/liteos-music")
QUERY = sys.argv[2] if len(sys.argv) > 2 else "love"
SOURCE = sys.argv[3] if len(sys.argv) > 3 else "netease"  # "netease" | "qq"

BOOT_MARKERS = (
    "compositor: mode",
    "compositor: desktop connected",
    "compositor: desktop first scene presented",
    # The startup splash animation owns pointer targeting until this settles;
    # clicking before it lands on the wrong element (the splash).
    "lite-ui: desktop startup motion settled",
)
# The music player launches from the bottom dock (4th icon, the teal chart /
# monitor.png glyph), not a desktop icon. Fractions of the logical viewport.
DOCK_MUSIC = (1054 / 2000, 1042 / 1125)
APP_READY = "lite-ui: app music-player ready"

# QMP qcodes for typing an ASCII query. Space maps to "spc".
QCODE = {**{c: c for c in "abcdefghijklmnopqrstuvwxyz0123456789"}, " ": "spc"}


def main() -> int:
    OUT.mkdir(parents=True, exist_ok=True)
    if not IMAGE.is_file():
        print(f"missing {IMAGE}; run make sync-userland ARCH=aarch64", file=sys.stderr)
        return 2
    tmp = tempfile.TemporaryDirectory(prefix="liteos-music-")
    qmp_socket = Path(tmp.name) / "qmp.sock"
    command = _qemu_command(IMAGE, 1, interactive_devices=True, qmp_socket=qmp_socket)
    command.append("-snapshot")
    process = subprocess.Popen(
        command, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    assert process.stdout is not None
    output = bytearray()
    lock = threading.Lock()
    stop = threading.Event()

    def reader() -> None:
        while not stop.is_set():
            ready, _, _ = select.select([process.stdout], [], [], 0.1)
            if not ready:
                if process.poll() is not None:
                    return
                continue
            chunk = os.read(process.stdout.fileno(), 16 * 1024)
            if not chunk:
                return
            with lock:
                output.extend(chunk)

    def text() -> str:
        with lock:
            return ANSI.sub("", bytes(output).decode(errors="replace"))

    def wait(markers, phase, budget_s) -> None:
        deadline = time.monotonic() + budget_s
        while time.monotonic() < deadline:
            current = text()
            if "panicked at" in current:
                tail = "\n".join(current.splitlines()[-30:])
                raise RuntimeError(f"guest fatal during {phase}\n{tail}")
            if all(m in current for m in markers):
                return
            if process.poll() is not None:
                raise RuntimeError(f"QEMU exited during {phase}")
            time.sleep(0.1)
        missing = [m for m in markers if m not in text()]
        tail = "\n".join(text().splitlines()[-40:])
        raise RuntimeError(f"timed out during {phase}; missing={missing!r}\n{tail}")

    thread = threading.Thread(target=reader, daemon=True)
    thread.start()
    qmp = None
    try:
        wait(BOOT_MARKERS, "desktop boot", 120.0)
        qmp = QmpClient(qmp_socket)

        def shot(name):
            ppm = Path(tmp.name) / f"{name}.ppm"
            ppm.unlink(missing_ok=True)
            qmp._execute("screendump", {"filename": str(ppm)})
            for _ in range(50):
                if ppm.exists() and ppm.stat().st_size > 0:
                    break
                time.sleep(0.1)
            _ppm_to_png(ppm, OUT / f"{name}.png")
            print(f"shot {name}: {OUT / f'{name}.png'}")

        def move(xf, yf):
            qmp.move_abs(xf, yf)

        def click(xf, yf):
            move(xf, yf)
            qmp.button("left", True)
            qmp.button("left", False)
            time.sleep(0.4)

        def double_click(xf, yf):
            move(xf, yf)
            for _ in range(2):
                qmp.button("left", True)
                qmp.button("left", False)
            time.sleep(0.5)

        def type_text(s):
            for ch in s.lower():
                qcode = QCODE.get(ch)
                if not qcode:
                    continue
                qmp.key(qcode, True)
                qmp.key(qcode, False)
                time.sleep(0.05)

        # Open the music player from the dock (single click launches).
        click(*DOCK_MUSIC)
        wait((APP_READY,), "music-player launch", 25.0)
        time.sleep(1.5)
        shot("01-opened")

        # Non-maximized default window (~(315..1490)x(160..950) in the 2000-wide
        # display). Search input center ~x=0.485 y=0.267; Search button ~x=0.72.
        # Select the QQ Music source tab first when requested (~x=0.25 y=0.267).
        if SOURCE == "qq":
            click(0.25, 0.267)
            time.sleep(0.3)
        click(0.485, 0.267)
        type_text(QUERY)
        time.sleep(0.3)
        shot("02-typed")

        click(0.72, 0.267)
        time.sleep(1.0)
        shot("03a-just-searched")
        time.sleep(7.0)
        shot("03-results")

        # First result's Play button in the default window (~x=0.717 y=0.348).
        click(0.717, 0.348)
        time.sleep(1.0)
        shot("03b-just-clicked-play")
        time.sleep(9.0)
        shot("04-nowplaying")
        time.sleep(8.0)
        shot("05-playing")

        # Surface any net diagnostics from the serial log.
        log = text()
        (OUT / "serial.log").write_text(log)
        print(f"serial log: {OUT / 'serial.log'}")
        for line in log.splitlines():
            if any(k in line for k in ("lite-ui: app", "LITE_AUDIO", "net", "panic", "rustls", "resolve")):
                print("SERIAL:", line)
        return 0
    finally:
        stop.set()
        thread.join(timeout=2)
        if qmp is not None:
            qmp.close()
        terminate(process)
        tmp.cleanup()


def _ppm_to_png(ppm, png):
    try:
        from PIL import Image
        Image.open(ppm).convert("RGB").save(png)
    except Exception:
        import shutil
        for tool in ("magick", "convert", "pnmtopng"):
            if shutil.which(tool):
                subprocess.run([tool, str(ppm), str(png)], check=False)
                return
        shutil.copy2(ppm, png.with_suffix(".ppm"))


if __name__ == "__main__":
    raise SystemExit(main())
