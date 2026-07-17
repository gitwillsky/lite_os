#!/usr/bin/env python3
"""Deterministic QEMU VNC adapter for the desktop runtime gate."""

from __future__ import annotations

import os
import re
import select
import shutil
import socket
import struct
import subprocess
import tempfile
import time
from pathlib import Path

from qemu_gate import SERIAL_TRIGGER_SETTLE_SECONDS, send_interaction, terminate

ROOT = Path(__file__).resolve().parent.parent
ANSI = re.compile(r"\x1b\[[0-9;]*m")


class RfbClient:
    """Minimal RFB 3.8 client exposing only pointer and desktop-size input."""

    def __init__(self, port: int) -> None:
        deadline = time.monotonic() + 10
        connection = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        while True:
            try:
                connection.connect(("127.0.0.1", port))
                break
            except ConnectionRefusedError:
                if time.monotonic() >= deadline:
                    connection.close()
                    raise RuntimeError("QEMU VNC socket did not become ready")
                time.sleep(0.01)
        connection.settimeout(5)
        self.connection = connection
        self.width = 0
        self.height = 0
        self.drag_endpoint: tuple[int, int] | None = None
        self._handshake()

    def _receive(self, length: int) -> bytes:
        output = bytearray()
        while len(output) < length:
            chunk = self.connection.recv(length - len(output))
            if not chunk:
                raise RuntimeError("QEMU VNC closed during RFB handshake")
            output.extend(chunk)
        return bytes(output)

    def _handshake(self) -> None:
        version = self._receive(12)
        if not version.startswith(b"RFB 003."):
            raise RuntimeError(f"unsupported QEMU RFB version: {version!r}")
        self.connection.sendall(b"RFB 003.008\n")
        count = self._receive(1)[0]
        if count == 0:
            length = struct.unpack(">I", self._receive(4))[0]
            raise RuntimeError(self._receive(length).decode(errors="replace"))
        security = self._receive(count)
        if 1 not in security:
            raise RuntimeError("QEMU VNC did not offer local no-auth security")
        self.connection.sendall(b"\x01")
        if self._receive(4) != b"\0\0\0\0":
            raise RuntimeError("QEMU VNC rejected local no-auth security")
        self.connection.sendall(b"\x01")
        header = self._receive(24)
        self.width, self.height = struct.unpack(">HH", header[:4])
        name_length = struct.unpack(">I", header[20:24])[0]
        self._receive(name_length)
        # ExtendedDesktopSize (-308) authorizes the client SetDesktopSize message.
        self.connection.sendall(struct.pack(">BBHi", 2, 0, 1, -308))

    def pointer_sweep(self) -> None:
        positions = (
            (self.width // 4, self.height // 4),
            (self.width * 3 // 4, self.height // 4),
            (self.width * 3 // 4, self.height * 3 // 4),
            (self.width // 4, self.height * 3 // 4),
        )
        for index in range(64):
            x, y = positions[index % len(positions)]
            self.connection.sendall(struct.pack(">BBHH", 5, 0, x, y))

    def drag_window(self) -> None:
        """Drag the primary desktop window through the real tablet/button path."""
        title_x = self.width // 2
        title_y = min(90, self.height // 4)
        self.connection.sendall(struct.pack(">BBHH", 5, 0, title_x, title_y))
        self.connection.sendall(struct.pack(">BBHH", 5, 1, title_x, title_y))
        for offset in range(8, 129, 8):
            self.connection.sendall(
                struct.pack(">BBHH", 5, 1, title_x + offset, title_y + offset // 2)
            )
            time.sleep(0.004)
        self.drag_endpoint = (title_x + 128, title_y + 64)

    def release_window(self) -> None:
        """Commit the active drag at its last preview position."""
        if self.drag_endpoint is None:
            raise RuntimeError("window release requested without an active drag")
        x, y = self.drag_endpoint
        self.connection.sendall(struct.pack(">BBHH", 5, 0, x, y))
        self.drag_endpoint = None

    def type_key(self, keysym: int) -> None:
        """Send one press/release pair through the real VirtIO keyboard path."""
        for pressed in (1, 0):
            self.connection.sendall(struct.pack(">BBHI", 4, pressed, 0, keysym))

    def resize(self, width: int, height: int) -> None:
        if not 1 <= width <= 0xFFFF or not 1 <= height <= 0xFFFF:
            raise ValueError("RFB desktop size is outside the protocol range")
        message = struct.pack(
            ">BBHHBBIHHHHI",
            251,
            0,
            width,
            height,
            1,
            0,
            0,
            0,
            0,
            width,
            height,
            0,
        )
        self.connection.sendall(message)
        self.width = width
        self.height = height

    def close(self) -> None:
        self.connection.close()


def reserve_vnc_display() -> tuple[int, int]:
    """Return an available QEMU VNC display number and its loopback TCP port."""
    for display in range(100):
        port = 5900 + display
        probe = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            probe.bind(("127.0.0.1", port))
        except OSError:
            probe.close()
            continue
        probe.close()
        return display, port
    raise RuntimeError("no loopback VNC display is available")


def run(
    image: Path,
    markers: tuple[str, ...],
    forbidden_markers: tuple[str, ...],
    timeout_seconds: int = 60,
) -> None:
    """Boot the real desktop and drive its input/resize path through QEMU VNC."""
    qemu = shutil.which("qemu-system-riscv64")
    if qemu is None:
        raise RuntimeError("qemu-system-riscv64 is required")
    with tempfile.TemporaryDirectory(prefix="liteos-desktop-gate-") as directory:
        workspace = Path(directory)
        private_image = workspace / image.name
        shutil.copyfile(image, private_image)
        vnc_display, vnc_port = reserve_vnc_display()
        command = [
            qemu,
            "-machine",
            "virt",
            "-global",
            "virtio-mmio.force-legacy=false",
            "-vnc",
            f"127.0.0.1:{vnc_display}",
            "-serial",
            "stdio",
            "-monitor",
            "none",
            "-m",
            "512M",
            "-smp",
            "8",
            "-rtc",
            "base=utc",
            "-bios",
            "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
            "-kernel",
            "target/riscv64gc-unknown-none-elf/debug/kernel",
            "-drive",
            f"file={private_image},if=none,format=raw,id=x0",
            "-device",
            "virtio-blk-device,drive=x0",
            "-object",
            "rng-random,filename=/dev/urandom,id=rng0",
            "-device",
            "virtio-rng-device,rng=rng0",
            "-device",
            "virtio-gpu-device,xres=1920,yres=1080",
            "-device",
            "virtio-keyboard-device",
            "-device",
            "virtio-tablet-device",
            "-netdev",
            "user,id=net0",
            "-device",
            "virtio-net-device,netdev=net0",
        ]
        process = subprocess.Popen(
            command,
            cwd=ROOT,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        assert process.stdin is not None and process.stdout is not None
        output = bytearray()
        cursor = 0
        phase = 0
        key_started = 0.0
        rfb: RfbClient | None = None
        deadline = time.monotonic() + timeout_seconds
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([process.stdout], [], [], 0.25)
                if ready:
                    chunk = os.read(process.stdout.fileno(), 16 * 1024)
                    if not chunk:
                        break
                    output.extend(chunk)
                    text = ANSI.sub("", output.decode(errors="replace"))
                    found = [marker for marker in forbidden_markers if marker in text]
                    if found:
                        raise RuntimeError(f"desktop guest reached forbidden markers: {found!r}")
                    if phase == 0 and "Enter 'help' for a list of built-in commands." in text:
                        time.sleep(SERIAL_TRIGGER_SETTLE_SECONDS)
                        send_interaction(process.stdin, b"/bin/sh /run/verify-desktop.sh\n")
                        cursor = len(text)
                        phase = 1
                    elif phase == 1:
                        offset = text.find("LITEOS_DESKTOP_POINTER_ARMED", cursor)
                        if offset >= 0:
                            rfb = RfbClient(vnc_port)
                            rfb.pointer_sweep()
                            time.sleep(SERIAL_TRIGGER_SETTLE_SECONDS)
                            send_interaction(process.stdin, b"pointer\n")
                            cursor = offset + len("LITEOS_DESKTOP_POINTER_ARMED")
                            phase = 2
                    elif phase == 2:
                        offset = text.find("LITEOS_DESKTOP_DRAG_ARMED", cursor)
                        if offset >= 0:
                            assert rfb is not None
                            rfb.drag_window()
                            time.sleep(SERIAL_TRIGGER_SETTLE_SECONDS)
                            send_interaction(process.stdin, b"drag\n")
                            cursor = offset + len("LITEOS_DESKTOP_DRAG_ARMED")
                            phase = 3
                    elif phase == 3:
                        offset = text.find("LITEOS_DESKTOP_RELEASE_ARMED", cursor)
                        if offset >= 0:
                            assert rfb is not None
                            rfb.release_window()
                            time.sleep(SERIAL_TRIGGER_SETTLE_SECONDS)
                            send_interaction(process.stdin, b"release\n")
                            cursor = offset + len("LITEOS_DESKTOP_RELEASE_ARMED")
                            phase = 4
                    elif phase == 4:
                        offset = text.find("LITEOS_DESKTOP_KEY_ARMED", cursor)
                        if offset >= 0:
                            assert rfb is not None
                            key_started = time.monotonic()
                            rfb.type_key(ord("a"))
                            send_interaction(process.stdin, b"key\n")
                            cursor = offset + len("LITEOS_DESKTOP_KEY_ARMED")
                            phase = 5
                    elif phase == 5:
                        offset = text.find("LITEOS_DESKTOP_KEY_OK", cursor)
                        if offset >= 0:
                            latency_ms = (time.monotonic() - key_started) * 1000
                            if latency_ms > 50:
                                raise RuntimeError(
                                    f"desktop key-to-visible latency exceeded 50ms: {latency_ms:.1f}ms"
                                )
                            cursor = offset + len("LITEOS_DESKTOP_KEY_OK")
                            phase = 6
                    elif phase == 6:
                        offset = text.find("LITEOS_DESKTOP_RESIZE_ARMED", cursor)
                        if offset >= 0:
                            assert rfb is not None
                            for width, height in ((800, 600), (1024, 768), (1280, 720)):
                                rfb.resize(width, height)
                            time.sleep(SERIAL_TRIGGER_SETTLE_SECONDS)
                            send_interaction(process.stdin, b"resize\n")
                            cursor = offset + len("LITEOS_DESKTOP_RESIZE_ARMED")
                            phase = 7
                    if all(marker in text for marker in markers):
                        return
                if process.poll() is not None:
                    text = ANSI.sub("", output.decode(errors="replace"))
                    tail = "\n".join(text.splitlines()[-50:])
                    raise RuntimeError(
                        f"QEMU desktop process exited with {process.returncode}"
                        f"\n--- output tail ---\n{tail}"
                    )
        finally:
            if rfb is not None:
                rfb.close()
            terminate(process)
        text = ANSI.sub("", output.decode(errors="replace"))
        missing = [marker for marker in markers if marker not in text]
        tail = "\n".join(text.splitlines()[-50:])
        raise RuntimeError(
            f"desktop runtime gate failed; missing={missing!r}\n--- output tail ---\n{tail}"
        )
