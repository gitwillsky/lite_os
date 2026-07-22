#!/usr/bin/env python3
"""为启动围栏提供唯一的 QEMU 进程与输出判定实现。"""

from __future__ import annotations

import os
import re
import select
import shutil
import signal
import subprocess
import tempfile
import time
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
FATAL_LINE_DRAIN_SECONDS = 0.25


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
    image: Path, smp: int, interactive_devices: bool = False
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
        command.extend(
            [
                "-m",
                "512M",
                "-device",
                "virtio-gpu-device,xres=3008,yres=1692",
                "-device",
                "virtio-keyboard-device",
                "-device",
                "virtio-tablet-device",
            ]
        )
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


def boot(
    image: Path,
    smp: int,
    markers: tuple[str, ...],
    timeout_seconds: int = 30,
    interactions: tuple[tuple[str, bytes], ...] = (),
    forbidden_markers: tuple[str, ...] = (),
    persistent_writes: bool = False,
    interactive_devices: bool = False,
) -> None:
    """冷启动指定镜像，按 marker 注入输入，直到全部结果出现或 fail-stop。

    Args:
        image: 作为唯一 root block device 的 ext2 镜像。
        smp: QEMU 向 DTB 暴露的 hart 数。
        markers: 成功前必须全部出现的输出标记。
        timeout_seconds: 单次冷启动的 monotonic deadline 秒数。
        interactions: 按输出 marker 排序触发的终端输入。
        forbidden_markers: 任一出现即立即失败的输出标记。
        persistent_writes: 是否直接使用传入的一次性镜像；默认创建私有副本隔离 guest 写入。
        interactive_devices: 是否加入 run-gui 的 GPU、keyboard 与 tablet 设备拓扑。

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
    command = _qemu_command(image, smp, interactive_devices)
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
                        # marker 通常先于 ash 的下一条 prompt；立即注入会让 prompt 切断命令前缀。
                        # 空 data 只推进单调 cursor，是无需触碰 UART 的 ordering barrier。
                        time.sleep(SERIAL_TRIGGER_SETTLE_SECONDS)
                        send_interaction(process.stdin, data)
                if all(marker in text for marker in markers):
                    if "panicked at" in text or "[ERROR]" in text:
                        tail = "\n".join(text.splitlines()[-40:])
                        raise RuntimeError(
                            f"QEMU -smp {smp} reached a fatal/error path"
                            f"\n--- output tail ---\n{tail}"
                        )
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
        f"QEMU -smp {smp} boot gate failed; missing={missing!r}\n--- output tail ---\n{tail}"
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
