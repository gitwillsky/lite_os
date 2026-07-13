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
from pathlib import Path
from typing import BinaryIO

ROOT = Path(__file__).resolve().parent.parent
ANSI = re.compile(r"\x1b\[[0-9;]*m")
SERIAL_WRITE_CHUNK = 4
SERIAL_WRITE_INTERVAL_SECONDS = 0.001
SERIAL_TRIGGER_SETTLE_SECONDS = 0.02
SERIAL_ESCAPE_SETTLE_SECONDS = 0.1


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
    # raw-mode applet 的 ESC command sequence 不能依赖 canonical line buffering；逐字节注入可避免
    # 16550 FIFO 在 editor 尚未完成 mode transition 时吞掉尾部控制字符。普通 shell 命令仍使用批次。
    chunk_size = 1 if b"\x1b" in data else SERIAL_WRITE_CHUNK
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


def boot(
    image: Path,
    smp: int,
    markers: tuple[str, ...],
    timeout_seconds: int = 30,
    interactions: tuple[tuple[str, bytes], ...] = (),
    forbidden_markers: tuple[str, ...] = (),
    persistent_writes: bool = False,
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

    Returns:
        None；全部 marker 出现时返回。

    Raises:
        RuntimeError: QEMU 缺失、异常退出、超时或命中禁止标记。
    """
    qemu = shutil.which("qemu-system-riscv64")
    if not qemu:
        raise RuntimeError("qemu-system-riscv64 is required")
    private_directory: tempfile.TemporaryDirectory[str] | None = None
    if not persistent_writes:
        # QEMU snapshot 仍会申请 backing image 锁；私有副本才能与开发实例确定性隔离。
        # 缺失该分支时并行 `make run` 会让 gate 在进入 kernel 前因 fs.img 写锁失败。
        private_directory = tempfile.TemporaryDirectory(prefix="liteos-qemu-gate-")
        private_image = Path(private_directory.name) / image.name
        shutil.copyfile(image, private_image)
        image = private_image
    drive = f"file={image},if=none,format=raw,id=x0"
    command = [
        qemu,
        "-machine",
        "virt",
        "-nographic",
        "-smp",
        str(smp),
        "-rtc",
        "base=utc",
        "-bios",
        "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
        "-kernel",
        "target/riscv64gc-unknown-none-elf/debug/kernel",
        "-drive",
        drive,
        "-device",
        "virtio-blk-device,drive=x0",
        "-object",
        "rng-random,filename=/dev/urandom,id=rng0",
        "-device",
        "virtio-rng-device,rng=rng0",
        "-netdev",
        "user,id=net0",
        "-device",
        "virtio-net-device,netdev=net0",
    ]
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
                        raise RuntimeError(f"QEMU -smp {smp} reached a fatal/error path")
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
        command: shell 激活后执行且必须持续 mutation 的命令。
        active_marker: guest 确认 mutation loop 已开始的输出。
        delay_seconds: 观察到 active marker 后到 SIGKILL 的确定性延迟。
        timeout_seconds: 等待 console 与 active marker 的最大秒数。

    Returns:
        QEMU 被 SIGKILL 且 image 保留未 clean-shutdown 状态时返回。

    Raises:
        RuntimeError: QEMU 不可用、提前退出、超时或命中 kernel fatal path。
    """
    qemu = shutil.which("qemu-system-riscv64")
    if not qemu:
        raise RuntimeError("qemu-system-riscv64 is required")
    process = subprocess.Popen(
        [
            qemu,
            "-machine",
            "virt",
            "-nographic",
            "-smp",
            str(smp),
            "-rtc",
            "base=utc",
            "-bios",
            "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
            "-kernel",
            "target/riscv64gc-unknown-none-elf/debug/kernel",
            "-drive",
            f"file={image},if=none,format=raw,id=x0",
            "-device",
            "virtio-blk-device,drive=x0",
            "-object",
            "rng-random,filename=/dev/urandom,id=rng0",
            "-device",
            "virtio-rng-device,rng=rng0",
            "-netdev",
            "user,id=net0",
            "-device",
            "virtio-net-device,netdev=net0",
        ],
        cwd=ROOT,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    assert process.stdin is not None and process.stdout is not None
    output = bytearray()
    activated = False
    command_sent = False
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
                raise RuntimeError("power-cut guest reached a kernel fatal path")
            if not activated and "Please press Enter to activate this console." in text:
                time.sleep(SERIAL_TRIGGER_SETTLE_SECONDS)
                send_interaction(process.stdin, b"\n")
                activated = True
            if activated and not command_sent and "Enter 'help' for a list of built-in commands." in text:
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
