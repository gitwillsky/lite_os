#!/usr/bin/env python3
"""显示类运行时门禁的共用部分：QEMU（virtio-gpu、键盘、平板，无窗口）会话、QMP 与串口控制台。"""

from __future__ import annotations

import json
import os
import re
import select
import socket
import subprocess
import tempfile
import time
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

from qemu_gate import ANSI, _qemu_command, terminate

ROOT = Path(__file__).resolve().parent.parent
FATAL = ("panicked at", "[ERROR]", "compositor: error", "xkb-check: ", "wayland-probe: error", "shell-probe: error:")
TIMEOUT_SECONDS = 90


def read_ppm(path: Path) -> tuple[int, int, list[tuple[int, int, int]]]:
    """读取 QEMU `screendump` 的 P6 PPM。"""
    data = path.read_bytes()
    fields, offset = [], 0
    while len(fields) < 4:
        end = offset
        while data[end:end + 1].isspace():
            end += 1
        start = end
        while not data[end:end + 1].isspace():
            end += 1
        fields.append(data[start:end])
        offset = end
    if fields[0] != b"P6" or fields[3] != b"255":
        raise RuntimeError(f"unexpected screendump header {fields}")
    width, height = int(fields[1]), int(fields[2])
    raw = data[offset + 1:]
    if len(raw) != width * height * 3:
        raise RuntimeError("screendump is truncated")
    return width, height, [tuple(raw[i:i + 3]) for i in range(0, len(raw), 3)]


class Qmp:
    """最小 QMP 客户端：按 id 匹配响应，忽略异步事件。"""

    def __init__(self, path: Path) -> None:
        deadline = time.monotonic() + 30
        while True:
            try:
                self.socket = socket.socket(socket.AF_UNIX)
                self.socket.connect(str(path))
                break
            except OSError:
                self.socket.close()
                if time.monotonic() > deadline:
                    raise RuntimeError("QMP socket never appeared") from None
                time.sleep(0.1)
        self.stream = self.socket.makefile("rwb")
        self.stream.readline()
        self.execute("qmp_capabilities")

    def execute(self, command: str, arguments: dict | None = None):
        request = {"execute": command, "id": command}
        if arguments is not None:
            request["arguments"] = arguments
        self.stream.write(json.dumps(request).encode() + b"\n")
        self.stream.flush()
        while True:
            response = json.loads(self.stream.readline())
            if response.get("id") == command:
                if "error" in response:
                    raise RuntimeError(f"QMP {command}: {response['error']}")
                return response["return"]

    def close(self) -> None:
        self.socket.close()


class Console:
    """串口输出累积器：等待标记，遇到致命标记立即失败。"""

    def __init__(self, process: subprocess.Popen[bytes]) -> None:
        self.process, self.text = process, ""

    def mark(self) -> int:
        """当前输出长度；之后用 `after=` 只等待这个位置之后才出现的内容。"""
        return len(self.text)

    def wait_for(self, markers: tuple[str, ...], after: int = 0) -> None:
        """等待所有字面标记都出现在 `after` 之后的串口输出里。"""
        self._wait(lambda: all(marker in self.text[after:] for marker in markers), markers)

    def wait_for_match(self, pattern: str, after: int = 0) -> re.Match[str]:
        """等待正则整体匹配；用于需要取值的行（标记出现时该行可能还没完整到达）。"""
        self._wait(lambda: re.search(pattern, self.text[after:]) is not None, pattern)
        match = re.search(pattern, self.text[after:])
        assert match is not None
        return match

    def _wait(self, satisfied, description) -> None:
        deadline = time.monotonic() + TIMEOUT_SECONDS
        while not satisfied():
            if time.monotonic() > deadline:
                raise RuntimeError(f"timed out waiting for {description}\n{self.text[-1500:]}")
            ready, _, _ = select.select([self.process.stdout], [], [], 0.25)
            if ready:
                chunk = os.read(self.process.stdout.fileno(), 16 * 1024)
                if not chunk:
                    raise RuntimeError(f"QEMU exited early\n{self.text[-1500:]}")
                self.text += ANSI.sub("", chunk.decode(errors="replace"))
            for fatal in FATAL:
                if fatal in self.text:
                    raise RuntimeError(f"fatal marker `{fatal}`\n{self.text[-1500:]}")



class Session:
    """一次显示门禁：串口控制台、按需连接的 QMP，以及放截图的临时目录。"""

    def __init__(self, console: Console, qmp_path: Path, directory: Path) -> None:
        self.console, self.directory = console, directory
        self._qmp_path, self._qmp = qmp_path, None

    @property
    def qmp(self) -> Qmp:
        if self._qmp is None:
            self._qmp = Qmp(self._qmp_path)
        return self._qmp

    def screendump(self) -> tuple[int, int, list[tuple[int, int, int]]]:
        """经 QMP 取回当前 scanout，返回 `(宽, 高, RGB 像素)`。"""
        path = self.directory / "screen.ppm"
        self.qmp.execute("screendump", {"filename": str(path)})
        return read_ppm(path)


@contextmanager
def display_session(image: Path, memory: str = "2G") -> Iterator[Session]:
    """冷启动 `image` 的私有副本（guest 写入不污染基线）；退出时关闭 QMP 并终止 QEMU。"""
    with tempfile.TemporaryDirectory(prefix="liteos-display-gate-") as private:
        root = Path(private)
        private_image = root / image.name
        private_image.write_bytes(image.read_bytes())
        qmp_path = root / "display.qmp"
        command = _qemu_command(private_image, 2, memory=memory, display_qmp=qmp_path)
        process = subprocess.Popen(
            command, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True
        )
        session = Session(Console(process), qmp_path, root)
        try:
            yield session
        finally:
            if session._qmp is not None:
                session._qmp.close()
            terminate(process)
