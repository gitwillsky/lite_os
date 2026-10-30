#!/usr/bin/env python3
"""当前 UTM 开发实例的标准 QMP 入口、符号与串口命令调度。"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import select
import shlex
import shutil
import socket
import tempfile
import time
import uuid
from pathlib import Path
from typing import BinaryIO

from qemu_gate import ShellCommand, send_shell_interaction

SESSION = Path(__file__).resolve().parent.parent / "target/run/gui-debug.json"
MAX_OUTPUT = 1024 * 1024


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
        # 0700 系统临时目录同时限制 QMP/串口访问；不能使用监听所有网卡的端口。
        # UTM helper 受 App Sandbox 约束，普通进程的 /var/folders 临时目录不可访问。
        # 使用 UTM 自己的系统容器 tmp；短目录名还须满足 macOS sockaddr_un 的路径长度。
        scratch = Path.home() / "Library/Containers/com.utmapp.QEMUHelper/Data/tmp"
        self.temporary = tempfile.TemporaryDirectory(prefix="los-", dir=scratch)
        self.directory = Path(self.temporary.name)
        self.elf = self.directory / "kernel.elf"
        shutil.copyfile(elf, self.elf)
        self.command = socket.socket(socket.AF_UNIX)
        self.command.bind(str(self.directory / "shell.sock"))
        self.command.listen(4)
        self.command.setblocking(False)

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
            "shell": str(self.directory / "shell.sock"),
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
        self.command.close()
        try:
            try:
                if json.loads(SESSION.read_text())["directory"] == str(self.directory):
                    SESSION.unlink()
            except (FileNotFoundError, json.JSONDecodeError, KeyError):
                pass
        finally:
            self.temporary.cleanup()

    @staticmethod
    def reply(client: socket.socket, result: dict) -> None:
        try:
            client.settimeout(1)
            client.sendall(json.dumps(result, ensure_ascii=False).encode() + b"\n")
        except OSError:
            pass
        finally:
            client.close()

    def capture(self, serial: socket.socket, output: BinaryIO) -> None:
        """单一 owner 同时落盘 UART 与串行执行命令；超时不会伪称 guest 命令已取消。"""
        clients: dict[socket.socket, tuple[bytearray, float]] = {}
        active = None
        try:
            while True:
                readable, _, _ = select.select([serial, self.command, *clients], [], [], 0.25)
                for source in readable:
                    if source is self.command:
                        client, _ = self.command.accept()
                        client.setblocking(False)
                        clients[client] = (bytearray(), time.monotonic() + 5)
                    elif source is serial:
                        chunk = serial.recv(16 * 1024)
                        if not chunk:
                            return
                        output.write(chunk)
                        if active is None:
                            continue
                        active["buffer"].extend(chunk)
                        start = active["buffer"].find(active["start"])
                        end = active["buffer"].find(active["end"], max(0, start))
                        if end >= 0 and (start >= 0 or active["client"] is None):
                            suffix = active["buffer"].find(b"\x1f", end)
                            if suffix >= 0:
                                status = int(active["buffer"][end + len(active["end"]):suffix])
                                text = active["buffer"][start + len(active["start"]):end]
                                if active["client"] is not None:
                                    self.reply(active["client"], {
                                        "exit_code": status,
                                        "output": text.decode(errors="replace").strip("\r\n"),
                                    })
                                active = None
                        if active is not None and len(active["buffer"]) > MAX_OUTPUT:
                            if active["client"] is not None:
                                self.reply(active["client"], {
                                    "error": "output limit exceeded; command may still be running",
                                })
                            # 保留末尾用于识别结束标记；仍占用串口，不能与下一条命令交错。
                            active["client"] = None
                            del active["buffer"][:-256]
                    else:
                        buffer, _ = clients[source]
                        try:
                            chunk = source.recv(4096)
                        except OSError:
                            chunk = b""
                        buffer.extend(chunk)
                        if not chunk or len(buffer) > 65536:
                            self.reply(source, {"error": "incomplete or oversized request"})
                            del clients[source]
                        elif b"\n" in buffer:
                            del clients[source]
                            try:
                                request = json.loads(buffer.split(b"\n", 1)[0])
                                command = request["command"]
                                timeout = float(request["timeout"])
                                if not isinstance(command, str) or not 0 < timeout <= 300:
                                    raise ValueError("invalid command or timeout")
                                if not command or any(ord(c) < 0x20 or c == "\x7f" for c in command):
                                    raise ValueError(
                                        "one printable shell command is required; use semicolons between statements"
                                    )
                                if active is not None:
                                    raise RuntimeError("serial shell is busy; previous command has not finished")
                                token = uuid.uuid4().hex
                                active = {
                                    "client": source,
                                    "buffer": bytearray(),
                                    "start": f"\x1e{token}:start\x1f".encode(),
                                    "end": f"\x1e{token}:end ".encode(),
                                    "deadline": time.monotonic() + timeout,
                                }
                                # marker 的完整控制字节不出现在 UART 回显中；子 shell 的 exit 不退出登录 shell。
                                line = (
                                    f"printf '\\036%s:start\\037\\n' {token}; "
                                    f"sh -c {shlex.quote(command)} </dev/null; liteos_debug_rc=$?; "
                                    f"printf '\\036%s:end %d\\037\\n' {token} \"$liteos_debug_rc\"\n"
                                )
                                echoed = bytearray()
                                try:
                                    # 复用 UART 的回显流控 owner；盲发长命令会溢出 FIFO，ash 只收到残句。
                                    with (
                                        serial.makefile("wb", buffering=0) as writer,
                                        serial.makefile("rb", buffering=0) as reader,
                                    ):
                                        send_shell_interaction(
                                            writer, reader, echoed, ShellCommand(line.encode()).data,
                                            deadline=active["deadline"],
                                        )
                                finally:
                                    output.write(echoed)
                                    active["buffer"].extend(echoed)
                            except (KeyError, ValueError, TypeError, RuntimeError, OSError) as error:
                                self.reply(source, {"error": str(error)})
                                if active is not None and active["client"] is source:
                                    active["client"] = None
                now = time.monotonic()
                for client, (_, deadline) in list(clients.items()):
                    if now >= deadline:
                        self.reply(client, {"error": "request timed out"})
                        del clients[client]
                if active is not None and active["client"] is not None and now >= active["deadline"]:
                    self.reply(active["client"], {
                        "error": "guest command timed out; it may still be running; serial remains busy until completion",
                    })
                    active["client"] = None
        finally:
            for client in clients:
                client.close()
            if active is not None and active["client"] is not None:
                self.reply(active["client"], {"error": "runtime disconnected before command completion"})


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    commands.add_parser("status")
    monitor = commands.add_parser("qmp")
    monitor.add_argument("command")
    monitor.add_argument("arguments", nargs="?", default="{}", help="QMP arguments JSON")
    shell = commands.add_parser("exec")
    shell.add_argument("command")
    shell.add_argument("--timeout", type=float, default=10)
    commands.add_parser("symbols")
    args = parser.parse_args()
    try:
        info = json.loads(SESSION.read_text())
        if args.action == "exec":
            if not 0 < args.timeout <= 300:
                raise ValueError("timeout must be in (0, 300]")
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(args.timeout + 2)
                client.connect(info["shell"])
                client.sendall(json.dumps({"command": args.command, "timeout": args.timeout}).encode() + b"\n")
                with client.makefile("rb") as stream:
                    result = json.loads(stream.readline())
            print(json.dumps(result, ensure_ascii=False, indent=2))
            return int(result.get("exit_code", 1))
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
    except (OSError, ValueError, KeyError, RuntimeError) as error:
        print(json.dumps({"error": str(error)}, ensure_ascii=False))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
