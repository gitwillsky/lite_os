#!/usr/bin/env python3
"""为启动围栏提供唯一的 QEMU 进程与输出判定实现。"""

from __future__ import annotations

import os
import re
import select
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import json
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Mapping

from build_target import (
    Acceleration,
    acceleration_from_environment,
    target_from_environment,
)

ROOT = Path(__file__).resolve().parent.parent
ANSI = re.compile(r"\x1b\[[0-9;]*m")
SERIAL_WRITE_CHUNK = 1
SERIAL_WRITE_INTERVAL_SECONDS = 0.0001
SERIAL_TRIGGER_SETTLE_SECONDS = 0.02
SERIAL_ESCAPE_SETTLE_SECONDS = 0.1
SERIAL_ECHO_TIMEOUT_SECONDS = 5.0
FATAL_LINE_DRAIN_SECONDS = 0.25
# `boot` 用进度看门狗而非固定墙钟 deadline：只要 guest 还在稳定吐新 UART 输出就不判死，
# 连续 STALL_SECONDS 无任何新字节才算 hang。这样宿主负载高（并发多个 QEMU、8-hart 冷启动）
# 只会让 50+ 命令的串行往返整体变慢，不会把一个活着的 guest 误判为超时——消除 pexpect 时序
# flake。STALL_SECONDS 需容纳最慢的合法静默窗口（如 `sleep 2` + 8 spinner 占满单 vCPU 采样、
# 多 hart 冷启动首帧）。HARD_MULTIPLE 给宽松绝对上限做失控兜底（真死循环不会无限跑）。
STALL_SECONDS = 60.0
HARD_DEADLINE_MULTIPLE = 30
# marker 出现后等 ash 下一条 prompt 再注入命令，替代盲 sleep：宿主慢时 prompt 迟到，盲 sleep
# 会让命令前缀被切断，导致该命令的 marker 永不出现。等真实 prompt 是标准 pexpect 做法。
SHELL_PROMPT = "/ # "
PROMPT_WAIT_SECONDS = 20.0


@dataclass(frozen=True)
class QemuRuntime:
    """一次 runtime gate 的目标相关 QEMU identity。"""

    arch: str
    acceleration: Acceleration
    binary: str
    cpu: str
    machine: str
    kernel_elf: str
    kernel_boot_artifact: str
    bootloader: str | None


def qemu_runtime(
    environment: Mapping[str, str] | None = None,
) -> QemuRuntime:
    """解析 runtime gate 的唯一目标、加速器和产物路由。

    Raises:
        ValueError: ARCH/ACCEL 未知，或选择了 RISC-V 不支持的 HVF。
    """
    target = target_from_environment(environment)
    acceleration = acceleration_from_environment(environment)
    cpu = target.qemu_cpu(acceleration)
    bootloader = None
    if target.requires_bootloader:
        bootloader = (
            f"bootloader/target/{target.kernel_triple}/release/bootloader"
        )
    return QemuRuntime(
        arch=target.arch,
        acceleration=acceleration,
        binary=target.qemu_binary,
        cpu=cpu,
        machine=target.qemu_machine(acceleration),
        kernel_elf=target.kernel_elf(),
        kernel_boot_artifact=target.kernel_boot_artifact(),
        bootloader=bootloader,
    )


def _qemu_command(
    image: Path,
    smp: int,
    interactive_devices: bool = False,
    qmp_socket: Path | None = None,
    audio_output: Path | None = None,
    memory: str | None = None,
) -> list[str]:
    runtime = qemu_runtime()
    qemu = shutil.which(runtime.binary)
    if qemu is None:
        raise RuntimeError(f"{runtime.binary} is required")
    command = [
        qemu,
        "-machine",
        runtime.machine,
        "-cpu",
        runtime.cpu,
    ]
    command.extend(
        [
            "-global",
            "virtio-mmio.force-legacy=false",
            "-nographic",
            "-smp",
            str(smp),
            "-rtc",
            "base=utc",
        ]
    )
    if memory is not None and not interactive_devices:
        command.extend(("-m", memory))
    if runtime.bootloader is not None:
        command.extend(["-bios", runtime.bootloader])
    command.extend(
        [
            "-kernel",
            runtime.kernel_boot_artifact,
            "-drive",
            f"file={image},if=none,format=raw,id=x0",
            "-device",
            "virtio-blk-device,drive=x0",
            "-object",
            "rng-random,filename=/dev/urandom,id=rng0",
            "-device",
            "virtio-rng-device,rng=rng0",
        ]
    )
    if interactive_devices:
        audio_backend = (
            "none,id=audio0"
            if audio_output is None
            else (
                f"wav,id=audio0,path={audio_output},"
                "out.frequency=48000,out.channels=2,out.format=s16"
            )
        )
        command.extend(
            [
                "-m",
                memory or "2G",
                "-audiodev",
                audio_backend,
                "-device",
                "virtio-gpu-device,xres=3008,yres=1692",
                "-device",
                "virtio-keyboard-device",
                "-device",
                "virtio-tablet-device",
                "-device",
                "virtio-sound-device,id=audio-device,audiodev=audio0,streams=1",
            ]
        )
    if qmp_socket is not None:
        # QMP channel for the frame-timing gate's synthetic input driver. Idle
        # desktops emit no frames, so the gate drives real virtio input through
        # this socket to produce a measurable present stream.
        command.extend(["-qmp", f"unix:{qmp_socket},server=on,wait=off"])
    command.extend(
        [
            "-netdev",
            "user,id=net0",
            "-device",
            "virtio-net-device,netdev=net0",
        ]
    )
    return command


def cpu_topology_markers(cpu_count: int) -> tuple[str, str]:
    """构造 architecture-neutral CPU topology 启动契约。

    Args:
        cpu_count: QEMU 向 guest 暴露的 CPU 数量。

    Returns:
        logical topology 发布与全部 platform CPU online 的唯一 marker 集合。

    Raises:
        ValueError: CPU 数量不是正数。
    """
    if cpu_count <= 0:
        raise ValueError("CPU count must be positive")
    expected_mask = (1 << cpu_count) - 1
    return (
        f"logical CPU topology initialized: count={cpu_count},",
        f"all platform CPUs online: count={cpu_count}, mask={expected_mask:#x}",
    )


def send_interaction(stream: BinaryIO, data: bytes) -> None:
    """按 UART 可消费速率注入交互，避免 host pipe 瞬时写满 16550 RX FIFO。

    Args:
        stream: QEMU stdin 的唯一 binary pipe。
        data: 当前 marker 对应的完整终端输入。

    Returns:
        None；全部字节已按序 flush 后返回。
    """
    # QEMU stdio pipe 没有 guest UART 的硬件流控；一次写入长命令会让字符在 IRQ drain 前溢出，
    # ash 随后收到残缺引号并停在 continuation prompt，令 gate 误报 kernel 功能失败。
    # PL011/QEMU stdio 没有 hardware flow control，且 guest 会在 bounded deferred batch 中短暂
    # 屏蔽 IRQ；逐字节平滑到 10 KB/s（低于 115200 baud 的有效 byte rate），既保持 gate
    # 吞吐，也不会让 4-byte host burst 绕过 UART FIFO。raw-mode applet 的 ESC sequence 同样依赖这个顺序。
    chunk_size = SERIAL_WRITE_CHUNK
    for offset in range(0, len(data), chunk_size):
        if offset != 0 and (data[offset] == 0x1B or data[offset - 1] == 0x1B):
            time.sleep(SERIAL_ESCAPE_SETTLE_SECONDS)
        stream.write(data[offset : offset + chunk_size])
        stream.flush()
        if offset + chunk_size < len(data):
            time.sleep(SERIAL_WRITE_INTERVAL_SECONDS)


def _is_echo_paced_shell_command(data: bytes) -> bool:
    """Returns whether `data` is one complete printable shell command."""
    return (
        data.endswith(b"\n")
        and bool(data)
        and all(byte == 0x0A or 0x20 <= byte <= 0x7E for byte in data)
    )


def send_shell_interaction(
    input_stream: BinaryIO,
    output_stream: BinaryIO,
    output: bytearray,
    data: bytes,
) -> None:
    """Sends one shell command using terminal echo as UART flow control.

    Args:
        input_stream: QEMU stdin, which owns bytes sent to the guest UART.
        output_stream: QEMU stdout, which carries the guest terminal echo.
        output: The gate's complete output buffer; echoed bytes are appended here.
        data: One printable newline-terminated shell command.

    Returns:
        None after every byte has been observed through the guest terminal.

    Raises:
        ValueError: `data` is not a complete printable shell command.
        RuntimeError: The guest closes output or does not echo a byte in time.
    """
    if not _is_echo_paced_shell_command(data):
        raise ValueError("echo pacing requires one printable shell command")
    for offset, byte in enumerate(data):
        echo_cursor = len(output)
        input_stream.write(bytes((byte,)))
        input_stream.flush()
        deadline = time.monotonic() + SERIAL_ECHO_TIMEOUT_SECONDS
        while byte not in output[echo_cursor:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError(
                    f"guest stopped echoing UART input at shell byte {offset}"
                )
            ready, _, _ = select.select([output_stream], [], [], remaining)
            if not ready:
                continue
            chunk = os.read(output_stream.fileno(), 16 * 1024)
            if not chunk:
                raise RuntimeError("guest UART output closed during shell input")
            output.extend(chunk)


def terminate(process: subprocess.Popen[bytes]) -> None:
    """终止围栏创建的整个 QEMU process group。"""
    if process.poll() is not None:
        return

    def send(value: signal.Signals) -> None:
        if process.poll() is not None:
            return
        try:
            process_group = os.getpgid(process.pid)
            if process_group == process.pid:
                os.killpg(process_group, value)
                return
        except (ProcessLookupError, PermissionError):
            pass
        # macOS 可能在 child 退出竞态中拒绝 killpg；回退只作用于本 gate 创建的直接 child。
        # 缺少此分支会在成功 marker 已出现后把清理竞态误报为 kernel 启动失败。
        try:
            process.send_signal(value)
        except ProcessLookupError:
            pass

    send(signal.SIGTERM)
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        send(signal.SIGKILL)
        process.wait(timeout=3)


def drain_fatal_line(stream: BinaryIO, output: bytearray) -> None:
    """命中 fatal marker 后补齐当前串口日志行，保留可诊断的失败证据。

    Args:
        stream: QEMU stdout 的唯一 binary pipe。
        output: 已收集且包含 fatal marker 的输出缓冲区。

    Returns:
        当前行结束、QEMU 关闭 pipe 或 250ms 上限到达时返回。
    """
    if output.endswith(b"\n"):
        return
    deadline = time.monotonic() + FATAL_LINE_DRAIN_SECONDS
    remaining = 4096
    while remaining != 0 and time.monotonic() < deadline:
        ready, _, _ = select.select([stream], [], [], deadline - time.monotonic())
        if not ready:
            return
        chunk = os.read(stream.fileno(), remaining)
        if not chunk:
            return
        output.extend(chunk)
        remaining -= len(chunk)
        if b"\n" in chunk:
            return


def _wait_for_prompt(stream: BinaryIO, output: bytearray, cursor: int) -> None:
    """Waits until the ash prompt appears at/after `cursor`, so a command is
    injected only once the shell is ready to read it (not mid-boot-spew).

    Appends any bytes it reads to `output` (the caller re-scans it) and does NOT
    advance the interaction cursor: a following `/ # `-triggered interaction must
    still be able to match this same prompt. Bounded by `PROMPT_WAIT_SECONDS`; on
    timeout it returns anyway and the caller injects (degrading to the old blind
    behavior rather than hanging).
    """
    deadline = time.monotonic() + PROMPT_WAIT_SECONDS
    while time.monotonic() < deadline:
        text = ANSI.sub("", bytes(output).decode(errors="replace"))
        if text.find(SHELL_PROMPT, cursor) >= 0:
            return
        ready, _, _ = select.select([stream], [], [], deadline - time.monotonic())
        if not ready:
            break
        chunk = os.read(stream.fileno(), 16 * 1024)
        if not chunk:
            break
        output.extend(chunk)


def boot(
    image: Path,
    smp: int,
    markers: tuple[str, ...],
    timeout_seconds: int = 30,
    interactions: tuple[tuple[str, bytes], ...] = (),
    forbidden_markers: tuple[str, ...] = (),
    success_settle_seconds: float = 0.0,
    persistent_writes: bool = False,
    interactive_devices: bool = False,
    audio_output: Path | None = None,
    memory: str | None = None,
) -> None:
    """冷启动指定镜像，按 marker 注入输入，直到全部结果出现或 fail-stop。

    Args:
        image: 作为唯一 root block device 的 ext2 镜像。
        smp: QEMU 向 DTB 暴露的 hart 数。
        markers: 成功前必须全部出现的输出标记。
        timeout_seconds: 单次冷启动的 monotonic deadline 秒数。
        interactions: 按输出 marker 排序触发的终端输入。
        forbidden_markers: 任一出现即立即失败的输出标记。
        success_settle_seconds: 成功标记齐备后继续观察 forbidden marker 的时长。
        persistent_writes: 是否直接使用传入的一次性镜像；默认创建私有副本隔离 guest 写入。
        interactive_devices: 是否加入 run-gui 的 GPU、keyboard 与 tablet 设备拓扑。
        audio_output: interactive topology 的 WAV 输出；None 使用不会访问 host 声卡的 none backend。
        memory: 显式 Guest RAM；None 保留既有 runtime gate 的 QEMU 默认值。

    Returns:
        None；全部 marker 出现时返回。

    Raises:
        RuntimeError: QEMU 缺失、异常退出、超时或命中禁止标记。
    """
    private_directory: tempfile.TemporaryDirectory[str] | None = None
    if not persistent_writes:
        # QEMU snapshot 仍会申请 backing image 锁；私有副本才能与开发实例确定性隔离。
        # 缺失该分支时并行 `make run` 会让 gate 在进入 kernel 前因 fs.img 写锁失败。
        private_directory = tempfile.TemporaryDirectory(prefix="liteos-qemu-gate-")
        private_image = Path(private_directory.name) / image.name
        shutil.copyfile(image, private_image)
        image = private_image
    command = _qemu_command(
        image,
        smp,
        interactive_devices,
        audio_output=audio_output,
        memory=memory,
    )
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        stdin=subprocess.PIPE if interactions else None,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    assert process.stdout is not None
    output = bytearray()
    pending_interactions = list(interactions)
    interaction_cursor = 0
    success_seen_at: float | None = None
    # 进度看门狗：last_progress 每次读到新字节就刷新；只有连续 STALL_SECONDS 无新输出才判死。
    # hard_deadline 是失控兜底，正常路径永远够用。
    now = time.monotonic()
    last_progress = now
    hard_deadline = now + timeout_seconds * HARD_DEADLINE_MULTIPLE
    try:
        while True:
            now = time.monotonic()
            if now - last_progress >= STALL_SECONDS or now >= hard_deadline:
                break
            ready, _, _ = select.select([process.stdout], [], [], 0.25)
            if ready:
                chunk = os.read(process.stdout.fileno(), 16 * 1024)
                if not chunk:
                    break
                output.extend(chunk)
                last_progress = time.monotonic()
                text = ANSI.sub("", output.decode(errors="replace"))
                found = [marker for marker in forbidden_markers if marker in text]
                if found:
                    drain_fatal_line(process.stdout, output)
                    text = ANSI.sub("", output.decode(errors="replace"))
                    tail = "\n".join(text.splitlines()[-40:])
                    raise RuntimeError(
                        f"QEMU -smp {smp} reached forbidden markers: {found!r}"
                        f"\n--- output tail ---\n{tail}"
                    )
                while pending_interactions:
                    marker, data = pending_interactions[0]
                    marker_offset = text.find(marker, interaction_cursor)
                    if marker_offset < 0:
                        break
                    pending_interactions.pop(0)
                    # 每个 marker 只能消费上一交互之后的新输出；缺少 cursor 时，重复 prompt/
                    # Stopped 文本会立即触发未来输入，使 gate 绕过 guest 的真实状态转换。
                    interaction_cursor = marker_offset + len(marker)
                    assert process.stdin is not None
                    if data:
                        # marker 通常先于 ash 的下一条 prompt；等 prompt 出现再注入，避免宿主慢时
                        # prompt 迟到导致命令前缀被切断（盲 sleep 会漏掉该命令的 marker）。
                        # 若触发 marker 本身就是 prompt，说明 prompt 已到，直接注入不再等待。
                        # 不推进 cursor：紧随其后的 `/ # ` 触发型交互仍需匹配同一个 prompt。
                        if not marker.endswith(SHELL_PROMPT):
                            _wait_for_prompt(process.stdout, output, interaction_cursor)
                        if _is_echo_paced_shell_command(data):
                            send_shell_interaction(
                                process.stdin,
                                process.stdout,
                                output,
                                data,
                            )
                        else:
                            send_interaction(process.stdin, data)
                        # 等 prompt/注入期间读到的字节也是进度，刷新看门狗防误判 stall。
                        last_progress = time.monotonic()
                if all(marker in text for marker in markers):
                    if "panicked at" in text or "[ERROR]" in text:
                        tail = "\n".join(text.splitlines()[-40:])
                        raise RuntimeError(
                            f"QEMU -smp {smp} reached a fatal/error path"
                            f"\n--- output tail ---\n{tail}"
                        )
                    if success_seen_at is None:
                        success_seen_at = time.monotonic()
            if success_seen_at is not None and (
                time.monotonic() - success_seen_at >= success_settle_seconds
            ):
                return
            if process.poll() is not None:
                break
    finally:
        terminate(process)
        if private_directory is not None:
            private_directory.cleanup()

    text = ANSI.sub("", output.decode(errors="replace"))
    missing = [marker for marker in markers if marker not in text]
    tail = "\n".join(text.splitlines()[-40:])
    raise RuntimeError(
        f"QEMU -smp {smp} boot gate failed; returncode={process.returncode!r};"
        f" missing={missing!r}\n--- output tail ---\n{tail}"
    )


def power_cut(
    image: Path,
    smp: int,
    command: bytes,
    active_marker: str,
    delay_seconds: float,
    timeout_seconds: int = 30,
) -> None:
    """在 guest 持续执行 mutation 时 SIGKILL QEMU，模拟没有 clean shutdown 的掉电。

    Args:
        image: 直接承受 guest 写入的私有 root image。
        smp: QEMU 暴露的 hart 数。
        command: shell 激活后执行且必须持续 mutation 的命令；为空时 guest sysinit 自启动。
        active_marker: guest 确认 mutation loop 已开始的输出。
        delay_seconds: 观察到 active marker 后到 SIGKILL 的确定性延迟。
        timeout_seconds: 等待 console 与 active marker 的最大秒数。

    Returns:
        QEMU 被 SIGKILL 且 image 保留未 clean-shutdown 状态时返回。

    Raises:
        RuntimeError: QEMU 不可用、提前退出、超时或命中 kernel fatal path。
    """
    process = subprocess.Popen(
        _qemu_command(image, smp),
        cwd=ROOT,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    assert process.stdin is not None and process.stdout is not None
    output = bytearray()
    command_sent = not command
    deadline = time.monotonic() + timeout_seconds
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([process.stdout], [], [], 0.25)
            if not ready:
                if process.poll() is not None:
                    break
                continue
            chunk = os.read(process.stdout.fileno(), 16 * 1024)
            if not chunk:
                break
            output.extend(chunk)
            text = ANSI.sub("", output.decode(errors="replace"))
            if "panicked at" in text or "[ERROR]" in text:
                drain_fatal_line(process.stdout, output)
                text = ANSI.sub("", output.decode(errors="replace"))
                tail = "\n".join(text.splitlines()[-40:])
                raise RuntimeError(
                    "power-cut guest reached a kernel fatal path"
                    f"\n--- output tail ---\n{tail}"
                )
            # BusyBox help banner 先于真正的 prompt；desktop 与 shell 在这段窗口内仍可能
            # 竞争 console input。只有完整 prompt 出现后注入，缺失时 power-cut mutation 命令会
            # 被启动期 reader 吞掉，guest 随后永久停在空 shell。
            if not command_sent and "/ # " in text:
                time.sleep(SERIAL_TRIGGER_SETTLE_SECONDS)
                send_interaction(process.stdin, command)
                command_sent = True
            if command_sent and active_marker in text:
                time.sleep(delay_seconds)
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=3)
                return
    finally:
        terminate(process)
    text = ANSI.sub("", output.decode(errors="replace"))
    tail = "\n".join(text.splitlines()[-40:])
    raise RuntimeError(f"power-cut gate missed {active_marker!r}\n--- output tail ---\n{tail}")


# Absolute virtio-tablet axes are normalized to [0, 0x7FFF] across the display,
# per QEMU's `qemu_input_scale_axis`. The gate reasons in fractions of the
# 3008x1692 GUI and converts here.
QMP_ABS_MAX = 0x7FFF
FRAME_STATS_RE = re.compile(
    r"compositor: frame-stats window=(\d+) frames=(\d+) dropped=(\d+) "
    r"p50_us=(\d+) p95_us=(\d+) p99_us=(\d+)"
)


class QmpClient:
    """Minimal QMP client over a unix socket for synthetic input injection.

    Only the input and graceful-shutdown surfaces used by runtime gates are
    exposed; it is not a general QMP wrapper.
    """

    def __init__(self, path: Path, connect_timeout_s: float = 10.0) -> None:
        deadline = time.monotonic() + connect_timeout_s
        last_error: OSError | None = None
        # QEMU creates the server socket asynchronously after launch; retry the
        # connect until it appears or the deadline passes.
        while time.monotonic() < deadline:
            try:
                self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self._sock.connect(str(path))
                break
            except OSError as error:
                last_error = error
                time.sleep(0.1)
        else:
            raise RuntimeError(f"QMP socket never became connectable: {last_error}")
        self._sock.settimeout(5.0)
        self._buffer = b""
        self._read_message()  # greeting
        self._execute("qmp_capabilities")

    def _read_message(self) -> dict:
        while b"\n" not in self._buffer:
            chunk = self._sock.recv(4096)
            if not chunk:
                raise RuntimeError("QMP socket closed mid-message")
            self._buffer += chunk
        line, self._buffer = self._buffer.split(b"\n", 1)
        return json.loads(line)

    def _execute(self, command: str, arguments: dict | None = None) -> None:
        request: dict = {"execute": command}
        if arguments is not None:
            request["arguments"] = arguments
        self._sock.sendall((json.dumps(request) + "\r\n").encode())
        # Drain until the matching return/error, skipping asynchronous events.
        while True:
            message = self._read_message()
            if "error" in message:
                raise RuntimeError(f"QMP {command} failed: {message['error']}")
            if "return" in message:
                return

    def _send_events(self, events: list[dict]) -> None:
        self._execute("input-send-event", {"events": events})

    def move_abs(self, x_fraction: float, y_fraction: float) -> None:
        """Moves the absolute pointer to a fraction of the display extent."""

        def axis(fraction: float) -> int:
            return round(max(0.0, min(1.0, fraction)) * QMP_ABS_MAX)

        self._send_events(
            [
                {"type": "abs", "data": {"axis": "x", "value": axis(x_fraction)}},
                {"type": "abs", "data": {"axis": "y", "value": axis(y_fraction)}},
            ]
        )

    def button(self, name: str, down: bool) -> None:
        self._send_events([{"type": "btn", "data": {"button": name, "down": down}}])

    def key(self, qcode: str, down: bool) -> None:
        """Presses or releases one key by QMP qcode (e.g. "esc", "ret")."""
        self._send_events(
            [{"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": qcode}}}]
        )

    def quit(self) -> None:
        """Requests graceful QEMU shutdown.

        Returns:
            After QMP accepts the request. The caller still owns waiting for
            the QEMU process to exit.

        Raises:
            RuntimeError: QMP rejects the request or closes before replying.
        """
        self._execute("quit")

    def stop_and_unrealize(self, device_id: str) -> None:
        """Stops the VM and completes one device's backend cleanup.

        Args:
            device_id: QOM identity assigned by the matching ``-device id=``.

        Returns:
            After QMP accepts ``stop`` and QOM finishes unrealizing the device.
            Stopping first prevents the guest from racing device teardown; a
            successful unrealize means the device backend cleanup has completed.

        Raises:
            RuntimeError: QMP rejects either lifecycle transition or closes
            before replying.
        """
        self._execute("stop")
        self._execute(
            "qom-set",
            {
                "path": f"/machine/peripheral/{device_id}",
                "property": "realized",
                "value": False,
            },
        )

    def close(self) -> None:
        try:
            self._sock.close()
        except OSError:
            pass


def start_frame_workload(qmp: QmpClient, duration_s: float, stop: "threading.Event") -> None:
    """Drives a sustained ~60Hz flip stream by resizing a window via React.

    The only architectural path that page-flips per event is a DESKTOP-side React
    scene commit. A titlebar drag installs a compositor move-grab that composites
    via DIRTYFB WITHOUT a page-flip, and app content only flips at the app's own
    commit rate — neither reliably yields 60Hz. But dragging a window's RESIZE
    grip stays entirely in desktop React: each pointer motion runs
    `continueResize` -> `onResize` -> `commitResize` (`move()` + `setOpen()`),
    which commits a fresh scene with pixels_changed=true -> compositor
    `accept_scene` -> `present_scene` -> one page-flip. The desktop's own resize
    throttle paces these to ~60Hz (RESIZE_FRAME_MS), which is exactly the cadence
    the gate measures.

    Presses the explicitly launched Terminal window's bottom-right (`se`) resize
    grip and oscillates the pointer in small steps for `duration_s`, then
    releases. Runs on its own thread; the caller's reader drains serial
    concurrently.
    """
    # `se` grip center: window logical bottom-right (870,570), grip is 8x8 at the
    # corner, center ~ (866,566) on a 1504x846 logical screen.
    grip_x, grip_y = 866 / 1504, 566 / 846
    qmp.move_abs(grip_x, grip_y)
    qmp.button("left", True)
    deadline = time.monotonic() + duration_s
    step = 0
    try:
        while time.monotonic() < deadline and not stop.is_set():
            # Oscillate the corner outward/inward by up to ~120 logical px so the
            # window genuinely resizes each motion (a zero-delta move commits no
            # new scene). Triangle wave keeps it on-screen and above MIN size.
            phase = (step % 40) / 40.0
            tri = phase if phase <= 0.5 else 1.0 - phase  # 0..0.5..0
            delta = tri * (120 / 1504)
            qmp.move_abs(grip_x + delta, grip_y + delta * (846 / 1504))
            step += 1
            time.sleep(0.004)  # inject fast; guest resize throttle paces commits
    finally:
        qmp.button("left", False)


def measure_frame_timing(
    image: Path,
    settle_s: float = 30.0,
    timeout_seconds: int = 120,
) -> dict[str, int]:
    """Cold-boots the desktop stack, self-drives frames, and returns frame stats.

    Boots with the interactive-device topology plus a QMP channel, waits for the
    empty desktop, explicitly opens Terminal and File Manager through desktop
    input, then drives resize commits on Terminal. A background thread drains
    serial the whole time so `compositor: frame-stats` markers are read as they
    arrive. Returns the WORST window (max dropped, then p99/p95) so a single good
    window cannot mask a bad one.

    Raises:
        RuntimeError: QEMU/QMP unavailable, boot markers missing, a fatal path,
            or no full frame-stats window was produced before the deadline.
    """
    private_directory = tempfile.TemporaryDirectory(prefix="liteos-frame-timing-")
    private_image = Path(private_directory.name) / image.name
    shutil.copyfile(image, private_image)
    qmp_socket = Path(private_directory.name) / "qmp.sock"
    boot_markers = (
        "init started: BusyBox v1.37.0",
        "compositor: mode",
        "compositor: desktop connected",
        "compositor: desktop first scene presented",
        "lite-ui: desktop ready",
    )
    process = subprocess.Popen(
        _qemu_command(private_image, 1, interactive_devices=True, qmp_socket=qmp_socket),
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    assert process.stdout is not None
    # Shared serial buffer drained by a background thread so the reader is never
    # starved while the guest self-drives frames.
    output = bytearray()
    output_lock = threading.Lock()
    stop_reading = threading.Event()

    def reader() -> None:
        while not stop_reading.is_set():
            ready, _, _ = select.select([process.stdout], [], [], 0.1)
            if not ready:
                if process.poll() is not None:
                    return
                continue
            chunk = os.read(process.stdout.fileno(), 16 * 1024)
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
    second_window_opened = False
    deadline = time.monotonic() + timeout_seconds
    try:
        # 1. Wait for the desktop stack to be fully up.
        while time.monotonic() < deadline:
            text = current_text()
            if "panicked at" in text or "[ERROR]" in text:
                tail = "\n".join(text.splitlines()[-40:])
                raise RuntimeError(
                    f"frame-timing guest reached a fatal path\n--- output tail ---\n{tail}"
                )
            if all(marker in text for marker in boot_markers):
                break
            if process.poll() is not None:
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("frame-timing gate timed out before desktop was ready")

        qmp = QmpClient(qmp_socket)
        # 2. The boot contract is an empty desktop. Launch both workload apps via
        #    real double-click input: Terminal is the second desktop icon and My
        #    Documents (the File Manager bundle) is the fourth.
        def double_click(x_fraction: float, y_fraction: float) -> None:
            qmp.move_abs(x_fraction, y_fraction)
            # Desktop icon hover/selection each publishes a new React hit tree.
            # Moving and pressing in the same host turn can route the first press
            # to the previous scene; the second press must likewise wait for the
            # selection scene or the double-click launch is intermittently lost.
            time.sleep(0.1)
            for click_index in range(2):
                qmp.button("left", True)
                qmp.button("left", False)
                if click_index == 0:
                    time.sleep(0.15)

        def wait_for(markers: tuple[str, ...], phase: str) -> None:
            phase_deadline = min(deadline, time.monotonic() + 15.0)
            while time.monotonic() < phase_deadline:
                text = current_text()
                if "panicked at" in text or "[ERROR]" in text:
                    tail = "\n".join(text.splitlines()[-40:])
                    raise RuntimeError(
                        f"frame-timing guest failed during {phase}"
                        f"\n--- output tail ---\n{tail}"
                    )
                if all(marker in text for marker in markers):
                    return
                time.sleep(0.1)
            missing = [marker for marker in markers if marker not in current_text()]
            raise RuntimeError(f"frame-timing gate missed {phase} markers: {missing!r}")

        double_click(47 / 1504, 92 / 846)
        wait_for(
            (
                "compositor: app 1 connected",
                "lite-ui: terminal session ready",
                "lite-ui: app terminal ready",
                "terminal-session: shell spawned",
            ),
            "Terminal launch",
        )
        double_click(47 / 1504, 212 / 846)
        wait_for(
            ("compositor: app 2 connected", "lite-ui: app file-manager ready"),
            "second app",
        )
        second_window_opened = True

        # 3. Activate the terminal window (app 1) so its resize grip is present,
        #    then drive a sustained resize-drag. The workload blocks for its
        #    duration while the background reader drains serial and collects the
        #    `compositor: frame-stats` markers the resize commits produce.
        qmp.move_abs(0.34, 0.11)  # terminal titlebar -> activate/raise it
        qmp.button("left", True)
        qmp.button("left", False)
        time.sleep(0.4)
        workload_deadline = min(deadline, time.monotonic() + settle_s)
        start_frame_workload(qmp, workload_deadline - time.monotonic(), stop_reading)

        # 4. Collect whatever windows the drive produced.
        windows = parse_windows(current_text())
        if not windows:
            # Give a last moment for a trailing marker to arrive/flush.
            time.sleep(0.5)
            windows = parse_windows(current_text())
        text_running = current_text()
        if "panicked at" in text_running or "[ERROR]" in text_running:
            tail = "\n".join(text_running.splitlines()[-40:])
            raise RuntimeError(
                f"frame-timing guest reached a fatal path\n--- output tail ---\n{tail}"
            )
    finally:
        stop_reading.set()
        reader_thread.join(timeout=2)
        if qmp is not None:
            qmp.close()
        terminate(process)
        text_final = current_text()
        private_directory.cleanup()
    if len(windows) < 2:
        tail = "\n".join(text_final.splitlines()[-40:])
        raise RuntimeError(
            "frame-timing gate needs one warmup and one steady frame-stats window "
            f"(collected={len(windows)}, second_window_opened={second_window_opened})"
            f"\n--- output tail ---\n{tail}"
        )
    # Discard the mandatory first window as warmup: it straddles desktop
    # settling, the second-window open and the focus clicks before the steady
    # resize stream begins, so its tail percentiles reflect startup gaps, not
    # steady cadence.
    steady = windows[1:]
    # Worst steady window: the gate must not be defeated by one good window
    # among bad.
    return max(steady, key=lambda w: (w["dropped"], w["p99_us"], w["p95_us"]))
