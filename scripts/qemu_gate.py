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

# guest 只执行一个自检脚本的 inittab 仍须先运行 rootfs 的 rcS：内核只挂载 / 与 /dev，
# /proc、/sys、/dev/pts 由 rcS 经 mount(2) 挂载。
GUEST_MOUNT_SYSINIT = "::sysinit:/etc/init.d/rcS\n"


def guest_inittab(command: str) -> str:
    """返回先挂载伪文件系统、再一次性运行 `command` 的 BusyBox inittab。"""
    return f"{GUEST_MOUNT_SYSINIT}::sysinit:{command}\n"
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
# 内核按 Linux `envp_init` 以 HOME=/ 启动 init，ash 的 `\w` 在 cwd 等于 $HOME 时显示 `~`，
# 因此 root 在 `/` 下的 prompt 是 `~ # `。
SHELL_PROMPT = "~ # "
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
    memory: str | None = None,
    kernel_arguments: str | None = None,
    scratch_disk: Path | None = None,
) -> list[str]:
    runtime = qemu_runtime()
    qemu = shutil.which(runtime.binary)
    if qemu is None:
        raise RuntimeError(f"{runtime.binary} is required")
    validate_kernel_boot_artifact(runtime)
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
    if memory is not None:
        command.extend(("-m", memory))
    if runtime.bootloader is not None:
        command.extend(["-bios", runtime.bootloader])
    root_device = (
        "-drive", f"file={image},if=none,format=raw,id=x0",
        "-device", "virtio-blk-device,drive=x0",
    )
    scratch_device = (
        "-drive", f"file={scratch_disk},if=none,format=raw,id=x1",
        "-device", "virtio-blk-device,drive=x1",
    ) if scratch_disk is not None else ()
    # guest 按 DTB transport 顺序装配；两个 virt machine 的节点顺序不同。
    # AArch64 先遇到后声明设备，RISC-V 先遇到先声明设备；都保证 root=vda、scratch=vdb。
    block_devices = (
        (*scratch_device, *root_device) if runtime.arch == "aarch64"
        else (*root_device, *scratch_device)
    )
    command.extend(
        [
            "-kernel",
            runtime.kernel_boot_artifact,
            *(("-append", kernel_arguments) if kernel_arguments is not None else ()),
            *block_devices,
            "-object",
            "rng-random,filename=/dev/urandom,id=rng0",
            "-device",
            "virtio-rng-device,rng=rng0",
            "-chardev",
            "qemu-vdagent,id=vdagent,name=vdagent,clipboard=on,mouse=off",
            "-device",
            "virtio-serial-device,id=virtio-serial0",
            "-device",
            "virtserialport,bus=virtio-serial0.0,chardev=vdagent,name=com.redhat.spice.0",
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


def validate_kernel_boot_artifact(runtime: QemuRuntime | None = None) -> Path:
    """验证实际启动比特与当前 ELF 对应；返回可用于内容寻址的启动产物。"""
    runtime = runtime or qemu_runtime()
    artifact = ROOT / runtime.kernel_boot_artifact
    if runtime.arch == "aarch64":
        # 延迟 import，artifact 工具的 userland recipe 也使用此 gate。
        from verify_artifacts import require_aarch64_raw_image

        require_aarch64_raw_image(ROOT / runtime.kernel_elf, artifact)
    elif not artifact.is_file():
        raise RuntimeError(f"missing kernel boot artifact: {artifact}")
    return artifact


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
    """Returns whether `data` is one complete printable UTF-8 shell command."""
    if not data.endswith(b"\n") or data.count(b"\n") != 1:
        return False
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        return False
    return all(
        character == "\n" or (ord(character) >= 0x20 and character != "\x7f")
        for character in text
    )


@dataclass(frozen=True)
class ShellCommand:
    """调用方声明的单条 shell 命令；UTF-8 printable 内容与最终 LF 在构造时校验。"""
    data: bytes

    def __post_init__(self) -> None:
        if not _is_echo_paced_shell_command(self.data):
            raise ValueError("shell command requires printable UTF-8 and final newline")


@dataclass(frozen=True)
class TerminalInput:
    """前台应用/终端控制输入；按实际 bytes 发送，不猜测 shell 或等待其 prompt。"""
    data: bytes


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
        None after all printable bytes are echoed and the final newline is submitted.

    Raises:
        ValueError: `data` is not a complete printable shell command.
        RuntimeError: The guest closes output or does not echo a byte in time.
    """
    if not _is_echo_paced_shell_command(data):
        raise ValueError("echo pacing requires one printable shell command")
    for offset, character in enumerate(data.decode("utf-8")):
        encoded = character.encode("utf-8")
        echo_cursor = len(output)
        send_interaction(input_stream, encoded)
        # 换行是提交 delimiter，不是可保证回显的 glyph。BusyBox lineedit 在列边界
        # goto_new_line 不再输出 LF；命令结果仍由 boot 的真实 marker/deadline 验证。
        if character == "\n":
            return
        deadline = time.monotonic() + SERIAL_ECHO_TIMEOUT_SECONDS
        while encoded not in output[echo_cursor:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError(
                    f"guest stopped echoing UART input at shell character {offset}"
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
    """等待最近 shell command 提交后的真实 prompt；cursor 是去 ANSI 的字符位置。

    后台 readiness 可晚于 prompt，不能把其位置当作 prompt 搜索起点。
    超时/EOF 明确失败；不退回盲发，也不消费 boot 的 readiness marker cursor。
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

    raise RuntimeError("guest shell prompt missing after latest command submission")


def send_triggered_interaction(
    input_stream: BinaryIO,
    output_stream: BinaryIO,
    output: bytearray,
    shell_cursor: int,
    data: ShellCommand | TerminalInput,
) -> None:
    """readiness 匹配后按实际 shell submission 等 prompt；控制键直接交给前台应用。"""
    if isinstance(data, ShellCommand):
        # 新 shell command 需要 prompt/echo flow control；Ctrl-C/Z 和编辑器输入的
        # readiness 来自前台应用 marker，等待 shell prompt 会阻止触发该 prompt 的操作。
        _wait_for_prompt(output_stream, output, shell_cursor)
        send_shell_interaction(input_stream, output_stream, output, data.data)
    elif isinstance(data, TerminalInput):
        send_interaction(input_stream, data.data)
    else:
        raise TypeError("serial interaction must declare ShellCommand or TerminalInput")


def boot(
    image: Path,
    smp: int,
    markers: tuple[str, ...],
    timeout_seconds: int = 30,
    interactions: tuple[tuple[str, ShellCommand | TerminalInput], ...] = (),
    forbidden_markers: tuple[str, ...] = (),
    success_settle_seconds: float = 0.0,
    persistent_writes: bool = False,
    memory: str | None = None,
    kernel_arguments: str | None = None,
    scratch_disk: Path | None = None,
) -> str:
    """冷启动指定镜像，按 marker 注入输入，直到全部结果出现或 fail-stop。

    Args:
        image: 作为唯一 root block device 的 ext4 镜像。
        smp: QEMU 向 DTB 暴露的 hart 数。
        markers: 成功前必须全部出现的输出标记。
        timeout_seconds: 单次冷启动的 monotonic deadline 秒数。
        interactions: 按输出 marker 排序触发的终端输入。
        forbidden_markers: 任一出现即立即失败的输出标记。
        success_settle_seconds: 成功标记齐备后继续观察 forbidden marker 的时长。
        persistent_writes: 是否直接使用传入的一次性镜像；默认创建私有副本隔离 guest 写入。
        memory: 显式 Guest RAM；None 保留既有 runtime gate 的 QEMU 默认值。
        kernel_arguments: 经 `-append` 写入 DTB `/chosen/bootargs` 的 kernel command line。
        scratch_disk: 作为第二块 virtio 盘（guest 内 `vdb`）附加的裸镜像；None 表示只有 root 盘。

    Returns:
        全部 marker 出现时返回去除 ANSI 后的完整串口文本；需要解析 guest 输出的诊断工具使用它，
        门禁调用者可以忽略。

    Raises:
        RuntimeError: QEMU 缺失、异常退出、超时或命中禁止标记。
    """
    if any(not isinstance(data, (ShellCommand, TerminalInput)) for _, data in interactions):
        raise TypeError("serial interaction must declare ShellCommand or TerminalInput")
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
        memory=memory,
        kernel_arguments=kernel_arguments,
        scratch_disk=scratch_disk,
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
    # 独立事实：readiness cursor 消费 marker；shell cursor 标记最近 command 提交。
    # 后台输出顺序不决定 shell 是否已有 prompt，缺失该边界会重复等待或重用旧 prompt。
    shell_cursor = 0
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
                    if data.data:
                        send_triggered_interaction(
                            process.stdin,
                            process.stdout,
                            output,
                            shell_cursor,
                            data,
                        )
                        # 等 prompt/注入期间读到的字节也是进度，刷新看门狗防误判 stall。
                        last_progress = time.monotonic()
                        # echo/prompt pacing 同样消费 stdout；必须立即解析它已经读入的
                        # marker，否则 guest 停在 prompt 时再无新字节唤醒外层 select。
                        text = ANSI.sub("", output.decode(errors="replace"))
                        if isinstance(data, ShellCommand):
                            shell_cursor = len(text)
                        found = [item for item in forbidden_markers if item in text]
                        if found:
                            drain_fatal_line(process.stdout, output)
                            tail = "\n".join(output.decode(errors="replace").splitlines()[-40:])
                            raise RuntimeError(
                                f"QEMU -smp {smp} reached forbidden markers: {found!r}"
                                f"\n--- output tail ---\n{tail}"
                            )
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
                return ANSI.sub("", output.decode(errors="replace"))
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
            if not command_sent and SHELL_PROMPT in text:
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
