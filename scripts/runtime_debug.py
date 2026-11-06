#!/usr/bin/env python3
"""通过 SSH 操作 LiteOS，并查询 UTM 宿主状态、设备和调试符号。"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shlex
import shutil
import socket
import struct
import subprocess
import tempfile
import time
import uuid
from pathlib import Path, PurePosixPath

SESSION = Path(__file__).resolve().parent.parent / "target/run/gui-debug.json"


def qmp(path: str, command: str, arguments: dict, timeout: float = 10) -> object:
    """完成标准 QMP 握手并匹配响应；异步事件不是命令结果。"""
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(timeout)
        connection.connect(path)
        with connection.makefile("rwb", buffering=0) as stream:
            greeting = json.loads(stream.readline())
            if "QMP" not in greeting:
                raise RuntimeError("not a QMP endpoint")
            result = None
            for request, values in (("qmp_capabilities", {}), (command, arguments)):
                identifier = uuid.uuid4().hex
                stream.write(json.dumps({"execute": request, "arguments": values, "id": identifier}).encode() + b"\n")
                deadline = time.monotonic() + timeout
                while True:
                    connection.settimeout(max(0.001, deadline - time.monotonic()))
                    line = stream.readline()
                    if not line:
                        raise RuntimeError("QMP connection closed")
                    response = json.loads(line)
                    if response.get("id") == identifier:
                        if "error" in response:
                            raise RuntimeError(f"QMP {request}: {response['error']}")
                        result = response["return"]
                        break
                    if time.monotonic() >= deadline:
                        raise TimeoutError(f"QMP {request} timed out")
            return result


class DebugSession:
    """run_gui 独占一个短命 session；不启动第二个 guest 或后台常驻服务。"""

    def __init__(self, elf: Path) -> None:
        # 0700 系统临时目录同时限制 QMP 访问；不能使用监听所有网卡的端口。
        # UTM helper 受 App Sandbox 约束，普通进程的 /var/folders 临时目录不可访问。
        # 使用 UTM 自己的系统容器 tmp；短目录名还须满足 macOS sockaddr_un 的路径长度。
        scratch = Path.home() / "Library/Containers/com.utmapp.QEMUHelper/Data/tmp"
        scratch.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(prefix="los-", dir=scratch)
        self.directory = Path(self.temporary.name)
        self.elf = self.directory / "kernel.elf"
        shutil.copyfile(elf, self.elf)

    def arguments(self) -> list[str]:
        """调试端点不带 -S；日常启动不会等待调试器，trace 默认没有启用事件。"""
        return [
            "-qmp", f"unix:{self.directory}/qmp.sock,server=on,wait=off",
            "-trace", f"file={self.directory}/qemu-trace.log",
        ]

    def publish(self, kernel: Path, image: Path, elf: Path, serial_log: Path) -> None:
        """只在本次串口已经连接后发布；符号文件用摘要约束，不能静默使用后来覆盖的 ELF。"""
        with self.elf.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        self.info = {
            "owner_pid": os.getpid(),
            "directory": str(self.directory),
            "qmp": str(self.directory / "qmp.sock"),
            "trace": str(self.directory / "qemu-trace.log"),
            "serial_log": str(serial_log),
            "kernel": str(kernel.resolve()),
            "image": str(image.resolve()),
            "elf": str(self.elf),
            "source_elf": str(elf.resolve()),
            "elf_sha256": digest,
        }
        SESSION.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(mode="w", dir=SESSION.parent, delete=False) as stream:
            json.dump(self.info, stream, indent=2)
            temporary = Path(stream.name)
        os.replace(temporary, SESSION)
        print(f"LiteOS debug session: {SESSION}", flush=True)

    def close(self) -> None:
        """只清理自己的入口；旧进程退出不能删掉下一次运行已发布的 session。"""
        try:
            try:
                if json.loads(SESSION.read_text())["directory"] == str(self.directory):
                    SESSION.unlink()
            except (FileNotFoundError, json.JSONDecodeError, KeyError):
                pass
        finally:
            self.temporary.cleanup()


def deploy(target: str, source: Path, destination: str, launch: bool) -> None:
    """Upload and verify a same-filesystem staging file before replacing an executable."""
    remote = PurePosixPath(destination)
    if (
        not remote.is_absolute() or str(remote) != destination or ".." in remote.parts
        or not remote.name or any(c.isspace() or c in "'\"\\:" for c in destination)
    ):
        raise ValueError("destination must be a normalized absolute executable path")
    with source.open("rb") as file:
        checksum = hashlib.file_digest(file, "sha256").hexdigest()
    ssh = ["ssh", "-oBatchMode=yes", "-oConnectTimeout=8", "--", target]
    stage = subprocess.check_output(
        [*ssh, "mktemp -d " + shlex.quote(str(remote.parent / ".liteos-deploy.XXXXXX"))],
        text=True,
    ).strip()
    stage_path = PurePosixPath(stage)
    if stage_path.parent != remote.parent or not stage_path.name.startswith(".liteos-deploy."):
        raise RuntimeError("unexpected remote staging path")
    staged = str(stage_path / "application")
    try:
        subprocess.run(["scp", "-q", "-B", "-oConnectTimeout=8", "-oServerAliveInterval=5",
                        "-oServerAliveCountMax=2", "--", str(source.resolve()), f"{target}:{staged}"], check=True)
        command = " && ".join((
            f"printf '%s  %s\\n' {checksum} {shlex.quote(staged)} | sha256sum -c -",
            f"chmod 755 {shlex.quote(staged)}",
            f"mv -fT {shlex.quote(staged)} {shlex.quote(destination)}",
        ))
        subprocess.run([*ssh, command], check=True)
        if launch:
            subprocess.run([*ssh, "test -S /run/liteos/wayland-0 || exit 1; "
                            "XDG_RUNTIME_DIR=/run/liteos nohup " + shlex.quote(destination)
                            + " </dev/null >>/root/.liteos-launch.log 2>&1 &"], check=True)
    finally:
        subprocess.run([*ssh, f"rm -f {shlex.quote(staged)}; rmdir {shlex.quote(stage)}"], check=False)


RFB_SOCKET = "/run/liteos/diagnostics/rfb.sock"
RFB_VERSION = b"RFB 003.008\n"
RFB_FORMAT = bytes((32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0))


def desktop(target: str, action: str, *, output: Path | None = None,
            position: tuple[int, int] = (0, 0), button: int = 0, keys: tuple[int, ...] = ()) -> None:
    """Use RFB over a standard SSH Unix socket forward, with no macOS window input."""
    with tempfile.TemporaryDirectory(prefix="liteos-rfb-", dir="/tmp") as temporary:
        path = Path(temporary) / "vnc"
        process = subprocess.Popen([
            "ssh", "-NT", "-oBatchMode=yes", "-oConnectTimeout=8", "-oExitOnForwardFailure=yes",
            "-L", f"{path}:{RFB_SOCKET}", "--", target,
        ], stdin=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 10
            while not path.exists():
                if process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("SSH desktop forward did not become ready")
                time.sleep(0.025)
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(10)
                client.connect(str(path))
                with client.makefile("rb") as stream:
                    def read(size: int) -> bytes:
                        data = stream.read(size)
                        if len(data) != size:
                            raise RuntimeError("RFB connection closed before a complete reply")
                        return data

                    if read(12) != RFB_VERSION:
                        raise RuntimeError("RFB 3.8 is required")
                    client.sendall(RFB_VERSION)
                    if 1 not in read(read(1)[0]):
                        raise RuntimeError("RFB did not offer authentication by SSH")
                    client.sendall(b"\x01")
                    if read(4) != bytes(4):
                        raise RuntimeError("RFB handshake failed")
                    client.sendall(b"\x01")
                    width, height = struct.unpack("!HH", read(4))
                    if read(16) != RFB_FORMAT:
                        raise RuntimeError("unexpected desktop pixel format")
                    name_size, = struct.unpack("!I", read(4))
                    if name_size > 1024 or width * height > 4096 * 4096:
                        raise RuntimeError("RFB desktop exceeds supported limits")
                    read(name_size)
                    if action == "key":
                        for down, symbols in ((1, keys), (0, reversed(keys))):
                            for symbol in symbols:
                                client.sendall(struct.pack("!BBHI", 4, down, 0, symbol))
                    if action == "pointer":
                        x, y = position
                        if not (0 <= x < width and 0 <= y < height):
                            raise ValueError("pointer coordinates outside the desktop")
                        for mask in (button, 0) if button else (0,):
                            client.sendall(struct.pack("!BBHH", 5, mask, x, y))
                    # A complete reply also acknowledges that preceding input was decoded.
                    w, h = (width, height) if output is not None else (1, 1)
                    client.sendall(struct.pack("!BBHHHH", 3, 0, 0, 0, w, h))
                    if read(4) != b"\0\0\0\1" or read(12) != struct.pack("!HHHHi", 0, 0, w, h, 0):
                        raise RuntimeError("unexpected RFB framebuffer update")
                    bgrx = read(w * h * 4)
                    if output is not None:
                        rgb = bytearray(w * h * 3)
                        for channel in range(3):
                            rgb[channel::3] = bgrx[2 - channel::4]
                        output.write_bytes(f"P6\n{w} {h}\n255\n".encode() + rgb)
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    commands.add_parser("status", help="UTM host-side instance status")
    monitor = commands.add_parser("qmp", help="UTM host and virtual hardware diagnostics")
    monitor.add_argument("command")
    monitor.add_argument("arguments", nargs="?", default="{}", help="QMP arguments JSON")
    commands.add_parser("symbols")
    update = commands.add_parser("deploy")
    update.add_argument("target", help="SSH destination or alias from ~/.ssh/config")
    update.add_argument("source", type=Path)
    update.add_argument("destination")
    update.add_argument("--launch", action="store_true")
    screen = commands.add_parser("screen")
    screen.add_argument("target")
    screen.add_argument("output", type=Path, help="PPM output path")
    pointer = commands.add_parser("pointer")
    pointer.add_argument("target")
    pointer.add_argument("x", type=int)
    pointer.add_argument("y", type=int)
    pointer.add_argument("--button", choices=("left", "middle", "right"))
    key = commands.add_parser("key")
    key.add_argument("target")
    key.add_argument("keysyms", nargs="+", type=lambda value: int(value, 0), help="X11 keysyms, pressed as a chord")
    args = parser.parse_args()
    try:
        if args.action == "deploy":
            deploy(args.target, args.source, args.destination, args.launch)
            return 0
        if args.action in ("screen", "pointer", "key"):
            if args.action == "screen":
                desktop(args.target, args.action, output=args.output)
            elif args.action == "pointer":
                desktop(args.target, args.action, position=(args.x, args.y),
                        button={None: 0, "left": 1, "middle": 2, "right": 4}[args.button])
            else:
                desktop(args.target, args.action, keys=tuple(args.keysyms))
            return 0
        info = json.loads(SESSION.read_text())
        if args.action == "symbols":
            qmp(info["qmp"], "query-status", {})
            with Path(info["elf"]).open("rb") as stream:
                if hashlib.file_digest(stream, "sha256").hexdigest() != info["elf_sha256"]:
                    raise RuntimeError("kernel ELF changed since boot; do not use these symbols")
            print(json.dumps({"elf": info["elf"], "elf_sha256": info["elf_sha256"]}, indent=2))
        elif args.action == "qmp":
            print(json.dumps(qmp(info["qmp"], args.command, json.loads(args.arguments)), indent=2))
        else:
            info["status"] = qmp(info["qmp"], "query-status", {})
            print(json.dumps(info, indent=2))
        return 0
    except (OSError, ValueError, KeyError, RuntimeError, struct.error, subprocess.CalledProcessError) as error:
        print(json.dumps({"error": str(error)}, ensure_ascii=False))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
