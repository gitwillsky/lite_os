#!/usr/bin/env python3
"""LiteOS 构建、运行与门禁 workflow 的唯一编排入口。

Make 只保留稳定的用户入口；依赖顺序、环境传递和 QEMU 参数在这里集中拥有。
这样同一个 scope 不会因为递归 ``make`` 重复解析并重复声明前置条件。
"""

from __future__ import annotations

import argparse
import os
import platform
import shutil
import signal
import subprocess
import sys
from pathlib import Path
from typing import Mapping, Sequence

from build_target import acceleration_from_environment, target_from_environment
from host_topology import default_guest_cpu_count

ROOT = Path(__file__).resolve().parent.parent
PYTHON = sys.executable


def _profile(environment: Mapping[str, str] | None = None) -> str:
    """返回经过白名单校验的 Cargo profile。"""
    source = os.environ if environment is None else environment
    value = source.get("PROFILE", "release")
    if value not in {"release", "debug"}:
        raise ValueError(f"PROFILE must be one of: release, debug; got {value!r}")
    return value


def _env_with(environment: Mapping[str, str] | None = None, **updates: str) -> dict[str, str]:
    """复制当前环境并覆盖 workflow 所需变量。"""
    result = dict(os.environ if environment is None else environment)
    result.update(updates)
    return result


def run(
    command: Sequence[str | Path],
    *,
    cwd: Path = ROOT,
    environment: Mapping[str, str] | None = None,
    capture: bool = False,
) -> str:
    """运行一个拥有明确 cwd/环境的 host 命令。

    Args:
        command: 不经过 shell 的 argv；避免 Make recipe 的 quoting 分叉。
        cwd: 命令工作目录。
        environment: 可选环境覆盖。
        capture: 是否返回 stdout；正常 workflow 输出直接流向终端。

    Returns:
        ``capture=True`` 时返回标准输出，否则返回空字符串。

    Raises:
        subprocess.CalledProcessError: 子命令非零退出。
        OSError: 工具缺失或进程无法启动。
    """
    result = subprocess.run(
        [str(argument) for argument in command],
        cwd=cwd,
        env=None if environment is None else dict(environment),
        check=True,
        stdout=subprocess.PIPE if capture else None,
        text=True,
    )
    return result.stdout if capture else ""


def python_script(name: str, *arguments: str | Path) -> list[str | Path]:
    """构造一个 repository Python script argv。"""
    return [PYTHON, ROOT / "scripts" / name, *arguments]


def target_paths(environment: Mapping[str, str] | None = None) -> dict[str, Path]:
    """返回一个环境下所有 workflow 共享的目标产物路径。"""
    source = os.environ if environment is None else environment
    target = target_from_environment(source)
    profile = _profile(source)
    arch = target.arch
    return {
        "kernel": ROOT / target.kernel_elf(profile),
        "boot": ROOT / target.kernel_boot_artifact(profile),
        "rootfs": ROOT / f"target/rootfs/{arch}.img",
        "fs": ROOT / f"fs-{arch}.img",
        "apk": ROOT / f"target/apk-apps/{arch}.img",
    }


def build_kernel(environment: Mapping[str, str] | None = None) -> None:
    """构建所选架构 kernel，并生成其启动 artifact。"""
    profile = _profile(environment)
    target = target_from_environment(environment)
    cargo_profile = ["--release"] if profile == "release" else []
    run(
        ["cargo", "build", "--target", target.kernel_triple, *cargo_profile],
        cwd=ROOT / "kernel",
        environment=environment,
    )
    run(python_script("verify_artifacts.py", "--build-boot-artifact", "--profile", profile), environment=environment)


def build_bootloader(environment: Mapping[str, str] | None = None) -> None:
    """仅在所选架构需要时构建 release bootloader。"""
    target = target_from_environment(environment)
    if target.requires_bootloader:
        run(["cargo", "build", "--release"], cwd=ROOT / "bootloader", environment=environment)


def build_musl(environment: Mapping[str, str] | None = None) -> None:
    """构建或命中架构隔离的 musl cache。"""
    run(python_script("verify_musl.py", "--build-only"), environment=environment)


def build_rootfs(environment: Mapping[str, str] | None = None) -> None:
    """按固定顺序准备 kernel、bootloader、musl 和产品 rootfs。"""
    build_kernel(environment)
    build_bootloader(environment)
    build_musl(environment)
    run(
        python_script("verify_busybox.py", "--build-only", "--image", target_paths(environment)["rootfs"]),
        environment=environment,
    )


def build_rust_std(environment: Mapping[str, str] | None = None) -> None:
    """构建 Rust std smoke 产物，不启动 guest。"""
    build_musl(environment)
    run(python_script("verify_rust_std.py", "--build-only"), environment=environment)


def reset_rootfs(environment: Mapping[str, str] | None = None, *, size_mib: str | None = None) -> None:
    """从只读基线原子重建可写开发镜像。"""
    build_rootfs(environment)
    paths = target_paths(environment)
    source_size = os.environ if environment is None else environment
    size = size_mib or source_size.get("FS_IMAGE_SIZE_MIB", "8192")
    temporary = paths["fs"].with_name(f".{paths['fs'].name}.{os.getpid()}.tmp")
    try:
        paths["fs"].parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(paths["rootfs"], temporary)
        run(
            python_script("resize_ext2_image.py", "--image", temporary, "--size-mib", size),
            environment=environment,
        )
        os.replace(temporary, paths["fs"])
    finally:
        temporary.unlink(missing_ok=True)


def prepare_rootfs(environment: Mapping[str, str] | None = None, *, size_mib: str | None = None) -> None:
    """保证开发镜像存在并在启动前只增长到目标容量。"""
    paths = target_paths(environment)
    source = os.environ if environment is None else environment
    size = size_mib or source.get("FS_IMAGE_SIZE_MIB", "8192")
    if not paths["fs"].is_file():
        reset_rootfs(environment, size_mib=size)
        return
    run(python_script("resize_ext2_image.py", "--image", paths["fs"], "--size-mib", size), environment=environment)


def sync_userland(environment: Mapping[str, str] | None = None, *, size_mib: str | None = None) -> None:
    """构建 musl、准备开发镜像并增量同步图形用户态。"""
    build_musl(environment)
    prepare_rootfs(environment, size_mib=size_mib)
    run(python_script("sync_userland.py", "--image", target_paths(environment)["fs"]), environment=environment)


def build_apk_apps(environment: Mapping[str, str] | None = None) -> None:
    """构建 APK 应用 image，并复用同一次 rootfs 准备。"""
    build_rootfs(environment)
    paths = target_paths(environment)
    run(
        python_script("verify_apk_apps.py", "--build-only", "--image", paths["rootfs"], "--output", paths["apk"]),
        environment=environment,
    )


def prepare_agent_development(environment: Mapping[str, str] | None = None) -> None:
    """准备只用于 Agent 的大容量 AArch64 开发镜像。"""
    source = os.environ if environment is None else environment
    if target_from_environment(source).arch != "aarch64":
        raise RuntimeError("Agent development currently supports only ARCH=aarch64")
    size = source.get("AGENT_FS_IMAGE_SIZE_MIB", "32768")
    build_kernel(environment)
    build_bootloader(environment)
    sync_userland(environment, size_mib=size)
    run(
        python_script(
            "prepare_agent_development.py",
            "--image",
            target_paths(environment)["fs"],
            "--size-mib",
            size,
            "--qemu-memory",
            source.get("AGENT_QEMU_MEMORY", "6G"),
        ),
        environment=environment,
    )


def _qemu_smp(source: Mapping[str, str]) -> str:
    """解析显式 QEMU_SMP，否则保留 Make 的宿主物理核策略。"""
    return source.get("QEMU_SMP") or str(default_guest_cpu_count())


def _qemu_command(
    image: Path,
    *,
    mode: str,
    memory: str,
    environment: Mapping[str, str] | None = None,
) -> list[str]:
    """构造 run/run-gui/run-gdb 共用的单一 QEMU 命令。"""
    source = os.environ if environment is None else environment
    target = target_from_environment(source)
    acceleration = acceleration_from_environment(source)
    qemu = shutil.which(target.qemu_binary)
    if qemu is None:
        raise RuntimeError(f"{target.qemu_binary} is required")
    command = [qemu, "-machine", target.qemu_machine(acceleration), "-cpu", target.qemu_cpu(acceleration)]
    command.extend(("-global", "virtio-mmio.force-legacy=false"))
    if mode == "gui":
        command.extend(("-display", source.get("QEMU_GUI_DISPLAY", "cocoa,zoom-to-fit=off")))
        command.extend(("-serial", f"file:{source.get('QEMU_GUI_SERIAL_LOG', 'target/run-gui-serial.log')}"))
        command.extend(("-monitor", "none"))
    else:
        command.append("-nographic")
    command.extend(("-m", memory, "-smp", _qemu_smp(source)))
    if mode != "gdb":
        command.extend(("-rtc", "base=utc"))
    if target.requires_bootloader:
        command.extend(("-bios", str(ROOT / "bootloader/target" / target.kernel_triple / "release/bootloader")))
    command.extend(
        (
            "-kernel",
            str(ROOT / target.kernel_boot_artifact(_profile(source))),
            "-drive",
            f"file={image},if=none,format=raw,id=x0",
            "-device",
            "virtio-blk-device,drive=x0",
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
        )
    )
    command.extend(("-device", source.get("QEMU_GPU_DEVICE", "virtio-gpu-device,xres=3008,yres=1692")))
    if mode == "gui":
        command.extend(("-device", "virtio-keyboard-device", "-device", "virtio-tablet-device"))
        if target.arch == "aarch64":
            command.extend(("-audiodev", "coreaudio,id=audio0,out.frequency=48000,out.channels=2"))
            command.extend(("-device", "virtio-sound-device,audiodev=audio0,streams=1"))
    elif mode == "run":
        if target.arch == "aarch64":
            command.extend(("-audiodev", "none,id=audio0", "-device", "virtio-sound-device,audiodev=audio0,streams=1"))
    command.extend(("-netdev", "user,id=net0", "-device", "virtio-net-device,netdev=net0"))
    if mode == "gdb":
        command.extend(("-S", "-s"))
    return command


def run_qemu(mode: str, environment: Mapping[str, str] | None = None, *, memory: str | None = None) -> None:
    """启动开发 QEMU；GUI 激活与 QEMU 生命周期由同一 workflow 拥有。"""
    source = os.environ if environment is None else environment
    if mode == "gdb":
        build_kernel(environment)
        build_bootloader(environment)
        prepare_rootfs(environment)
    else:
        build_kernel(environment)
        build_bootloader(environment)
        sync_userland(environment)
    paths = target_paths(environment)
    selected_memory = memory or source.get("QEMU_MEMORY", "2G")
    command = _qemu_command(paths["fs"], mode=mode, memory=selected_memory, environment=environment)
    if mode == "gui":
        serial_log = ROOT / source.get("QEMU_GUI_SERIAL_LOG", "target/run-gui-serial.log")
        serial_log.parent.mkdir(parents=True, exist_ok=True)
    process = subprocess.Popen(command, cwd=ROOT)
    activator: subprocess.Popen[bytes] | None = None
    if mode == "gui" and platform.system() == "Darwin":
        activator = subprocess.Popen(
            [
                "/usr/bin/osascript",
                str(ROOT / "scripts/activate_macos_process.applescript"),
                str(process.pid),
                source.get("QEMU_GUI_WINDOW_WIDTH", "1504"),
                source.get("QEMU_GUI_WINDOW_HEIGHT", "874"),
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    try:
        process.wait()
    except KeyboardInterrupt:
        process.send_signal(signal.SIGINT)
        process.wait()
    finally:
        if activator is not None and activator.poll() is None:
            activator.terminate()
    if process.returncode:
        raise subprocess.CalledProcessError(process.returncode, command)


def verify_unit(environment: Mapping[str, str] | None = None) -> None:
    """执行 kernel/architecture/syscall 与 user workspace 单元测试。"""
    run(
        [
            "cargo",
            "test",
            "-p",
            "architecture-check",
            "-p",
            "kernel-unit",
            "-p",
            "scheduler-unit",
            "-p",
            "syscall-abi",
        ],
        environment=environment,
    )
    assets = run(
        python_script("verify_busybox.py", "--build-ui-assets-only"),
        environment=environment,
        capture=True,
    ).strip()
    user_environment = _env_with(environment, LITE_UI_TEST_ASSETS=assets)
    run(["cargo", "test", "--manifest-path", "user/Cargo.toml", "--workspace"], environment=user_environment)


def verify_architecture_benchmark(environment: Mapping[str, str] | None = None) -> None:
    """执行 blocking architecture benchmark。"""
    run(["cargo", "run", "--quiet", "--release", "-p", "architecture-bench"], environment=environment)


def verify_architecture_release(environment: Mapping[str, str] | None = None) -> None:
    """构建 release kernel 并执行静态 façade/trap gate。"""
    target = target_from_environment(environment)
    run(["cargo", "build", "--target", target.kernel_triple, "--release"], cwd=ROOT / "kernel", environment=environment)
    run(python_script("verify_architecture_release.py"), environment=environment)
    run(python_script("check_trap_cost.py"), environment=environment)


def verify_runtime_gates(environment: Mapping[str, str] | None = None) -> None:
    """串行运行全部 QEMU runtime gate；并发由契约明确禁止。"""
    commands = [
        ["verify_boot.py", "--image", target_paths(environment)["rootfs"]],
        ["verify_musl.py"],
        ["verify_rust_std.py", "--image", target_paths(environment)["rootfs"]],
        ["verify_busybox.py", "--image", target_paths(environment)["rootfs"]],
        ["verify_apk_apps.py", "--image", target_paths(environment)["rootfs"]],
    ]
    for script, *arguments in commands:
        run(python_script(script, *arguments), environment=environment)
    target = target_from_environment(environment)
    acceleration = acceleration_from_environment(environment)
    if target.arch == "aarch64" and acceleration == "hvf":
        run(python_script("verify_audio.py", "--image", target_paths(environment)["rootfs"]), environment=environment)
    else:
        print(f"audio runtime gate skipped: {target.arch}/{acceleration} retains compile/static/boot coverage only")
    run(
        python_script("verify_frame_timing.py", "--image", target_paths(environment)["rootfs"]),
        environment=environment,
    )


def verify_runtime_audio(environment: Mapping[str, str] | None = None) -> None:
    """按架构/加速器契约执行 audio gate 或明确报告跳过。"""
    target = target_from_environment(environment)
    acceleration = acceleration_from_environment(environment)
    if target.arch == "aarch64" and acceleration == "hvf":
        run(python_script("verify_audio.py", "--image", target_paths(environment)["rootfs"]), environment=environment)
    else:
        print(f"audio runtime gate skipped: {target.arch}/{acceleration} retains compile/static/boot coverage only")


def verify_runtime_boot(environment: Mapping[str, str] | None = None) -> None:
    """执行 boot runtime gate，不隐式重建 rootfs。"""
    run(python_script("verify_boot.py", "--image", target_paths(environment)["rootfs"]), environment=environment)


def verify_runtime_frame_timing(environment: Mapping[str, str] | None = None) -> None:
    """执行真实 guest vblank frame-timing gate。"""
    run(
        python_script("verify_frame_timing.py", "--image", target_paths(environment)["rootfs"]),
        environment=environment,
    )


def verify_runtime_musl(environment: Mapping[str, str] | None = None) -> None:
    """执行 musl runtime gate，不隐式重建 rootfs。"""
    run(python_script("verify_musl.py"), environment=environment)


def verify_runtime_rust_std(environment: Mapping[str, str] | None = None) -> None:
    """执行 Rust std runtime gate，不隐式重建 rootfs。"""
    run(python_script("verify_rust_std.py", "--image", target_paths(environment)["rootfs"]), environment=environment)


def verify_runtime_busybox(environment: Mapping[str, str] | None = None) -> None:
    """执行 BusyBox runtime gate，不隐式重建 rootfs。"""
    run(python_script("verify_busybox.py", "--image", target_paths(environment)["rootfs"]), environment=environment)


def verify_runtime_apk_apps(environment: Mapping[str, str] | None = None) -> None:
    """执行 APK runtime gate，不隐式重建 rootfs。"""
    run(python_script("verify_apk_apps.py", "--image", target_paths(environment)["rootfs"]), environment=environment)


def verify_musl_scope(environment: Mapping[str, str] | None = None) -> None:
    """执行带构建前置条件的 musl smoke 公开目标。"""
    build_kernel(environment)
    build_bootloader(environment)
    run(python_script("verify_musl.py"), environment=environment)


def verify_rust_std_scope(environment: Mapping[str, str] | None = None) -> None:
    """构建一次产品产物后执行 Rust std runtime gate。"""
    build(environment)
    verify_runtime_rust_std(environment)


def verify_busybox_scope(environment: Mapping[str, str] | None = None) -> None:
    """构建一次产品产物后执行 BusyBox runtime gate。"""
    build(environment)
    verify_runtime_busybox(environment)


def verify_apk_apps_scope(environment: Mapping[str, str] | None = None) -> None:
    """构建一次产品产物后执行 APK runtime gate。"""
    build(environment)
    verify_runtime_apk_apps(environment)


def regenerate_terminal_font(environment: Mapping[str, str] | None = None) -> None:
    """生成并立即校验 terminal font checked asset。"""
    python = ROOT / "target/fontenv/bin/python"
    run([python, ROOT / "scripts/generate_terminal_font.py"], environment=environment)
    run([python, ROOT / "scripts/generate_terminal_font.py", "--verify"], environment=environment)


def regenerate_ui_font(environment: Mapping[str, str] | None = None) -> None:
    """生成 UI 比例字体 checked asset。"""
    run([ROOT / "target/fontenv/bin/python", ROOT / "scripts/generate_ui_font.py"], environment=environment)


def verify_host(environment: Mapping[str, str] | None = None) -> None:
    """执行 full verify 与 fast verify 共用的 host 检查。"""
    run(["cargo", "fmt", "--all", "--", "--check"], environment=environment)
    run(
        [
            "cargo",
            "clippy",
            "-p",
            "architecture-check",
            "-p",
            "architecture-bench",
            "-p",
            "kernel-unit",
            "-p",
            "scheduler-unit",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        environment=environment,
    )
    target = target_from_environment(environment)
    run(
        [
            "cargo",
            "clippy",
            "-p",
            "syscall-abi",
            "-p",
            "kernel",
            "--target",
            target.kernel_triple,
            "--bins",
            "--lib",
            "--",
            "-D",
            "warnings",
        ],
        environment=environment,
    )
    if target.requires_bootloader:
        run(["cargo", "clippy", "--release", "--", "-D", "warnings"], cwd=ROOT / "bootloader", environment=environment)
    verify_unit(environment)
    verify_architecture_benchmark(environment)


def verify_fast(environment: Mapping[str, str] | None = None) -> None:
    """只执行不启动 QEMU 的快速开发反馈 scope。"""
    verify_host(environment)


def verify_runtime(environment: Mapping[str, str] | None = None) -> None:
    """构建一次产品产物后串行执行 runtime gates。"""
    build(environment)
    verify_runtime_gates(environment)


def verify_riscv64_secondary(environment: Mapping[str, str] | None = None) -> None:
    """执行 AArch64 提交门禁附带的 RISC-V compile/static/boot smoke。"""
    secondary = _env_with(environment, ARCH="riscv64", ACCEL="tcg", PROFILE="release")
    run(python_script("workflow.py", "build"), environment=secondary)
    target = target_from_environment(secondary)
    run(
        [
            "cargo",
            "clippy",
            "-p",
            "syscall-abi",
            "-p",
            "kernel",
            "--target",
            target.kernel_triple,
            "--bins",
            "--lib",
            "--",
            "-D",
            "warnings",
        ],
        environment=secondary,
    )
    run(["cargo", "clippy", "--release", "--", "-D", "warnings"], cwd=ROOT / "bootloader", environment=secondary)
    run(python_script("verify_architecture_release.py"), environment=secondary)
    run(python_script("verify_artifacts.py"), environment=secondary)
    paths = target_paths(secondary)
    run(python_script("verify_boot.py", "--image", paths["rootfs"]), environment=secondary)
    run(python_script("verify_rust_std.py", "--image", paths["rootfs"]), environment=secondary)


def build(environment: Mapping[str, str] | None = None) -> None:
    """构建 kernel、bootloader 与产品 rootfs。"""
    build_rootfs(environment)


def verify(environment: Mapping[str, str] | None = None) -> None:
    """执行提交前完整门禁，保持 runtime 串行和双架构尾门禁。"""
    verify_host(environment)
    verify_architecture_release(environment)
    build(environment)
    run(["cargo", "run", "--quiet", "-p", "architecture-check"], environment=environment)
    run(python_script("verify_artifacts.py"), environment=environment)
    verify_runtime_gates(environment)
    if target_from_environment(environment).arch == "aarch64":
        verify_riscv64_secondary(environment)
    run(["git", "diff", "--check"], environment=environment)


def clean(environment: Mapping[str, str] | None = None) -> None:
    """清理 Cargo 构建和开发镜像，不触碰内容寻址 cache。"""
    run(["cargo", "clean"], environment=environment)
    run(["cargo", "clean"], cwd=ROOT / "bootloader", environment=environment)
    for image in (ROOT / "fs-aarch64.img", ROOT / "fs-riscv64.img"):
        image.unlink(missing_ok=True)


def clean_musl(environment: Mapping[str, str] | None = None) -> None:
    """清理 musl 构建 cache。"""
    shutil.rmtree(ROOT / "target/musl-runtime", ignore_errors=True)


def clean_busybox(environment: Mapping[str, str] | None = None) -> None:
    """清理 BusyBox 构建 cache。"""
    shutil.rmtree(ROOT / "target/busybox-runtime", ignore_errors=True)


def dispatch(scope: str, environment: Mapping[str, str] | None = None) -> None:
    """执行一个公开 workflow scope。"""
    simple = {
        "build-kernel": build_kernel,
        "build-bootloader": build_bootloader,
        "build-musl": build_musl,
        "build-rootfs": build_rootfs,
        "build-rust-std": build_rust_std,
        "build-apk-apps": build_apk_apps,
        "prepare-rootfs": prepare_rootfs,
        "reset-rootfs": reset_rootfs,
        "sync-userland": sync_userland,
        "prepare-agent-development": prepare_agent_development,
        "verify-unit": verify_unit,
        "verify-architecture-benchmark": verify_architecture_benchmark,
        "verify-architecture-release": verify_architecture_release,
        "verify-runtime-gates": verify_runtime_gates,
        "verify-runtime-boot": verify_runtime_boot,
        "verify-runtime-audio": verify_runtime_audio,
        "verify-runtime-frame-timing": verify_runtime_frame_timing,
        "verify-runtime-musl": verify_runtime_musl,
        "verify-runtime-rust-std": verify_runtime_rust_std,
        "verify-runtime-busybox": verify_runtime_busybox,
        "verify-runtime-apk-apps": verify_runtime_apk_apps,
        "verify-musl": verify_musl_scope,
        "verify-rust-std": verify_rust_std_scope,
        "verify-busybox": verify_busybox_scope,
        "verify-apk-apps": verify_apk_apps_scope,
        "verify-fast": verify_fast,
        "verify-runtime": verify_runtime,
        "verify-riscv64-secondary": verify_riscv64_secondary,
        "build": build,
        "verify": verify,
        "regen-font": regenerate_terminal_font,
        "regen-ui-font": regenerate_ui_font,
        "clean": clean,
        "clean-musl": clean_musl,
        "clean-busybox": clean_busybox,
    }
    if scope == "run":
        run_qemu("run", environment)
        return
    if scope == "run-gui":
        run_qemu("gui", environment)
        return
    if scope == "run-gdb":
        run_qemu("gdb", environment)
        return
    if scope == "run-agent-development":
        prepare_agent_development(environment)
        source = os.environ if environment is None else environment
        run_qemu("gui", environment, memory=source.get("AGENT_QEMU_MEMORY", "6G"))
        return
    if scope == "gdb":
        target = target_from_environment(environment)
        source = os.environ if environment is None else environment
        default_gdb = "aarch64-none-elf-gdb" if target.arch == "aarch64" else "riscv64-elf-gdb"
        tool = source.get("GDB", default_gdb).split()
        architecture = "aarch64" if target.arch == "aarch64" else "riscv:rv64"
        run(
            tool
            + [
                "-ex",
                f"file {ROOT / target.kernel_elf()}",
                "-ex",
                "target remote :1234",
                "-ex",
                f"set arch {architecture}",
            ],
            environment=environment,
        )
        return
    if scope == "addr2line":
        source = os.environ if environment is None else environment
        default_tool = (
            "aarch64-none-elf-addr2line"
            if target_from_environment(source).arch == "aarch64"
            else "riscv64-unknown-elf-addr2line"
        )
        tool = source.get("ADDR2LINE", default_tool)
        address = source.get("ADDR")
        if not address:
            raise RuntimeError("ADDR is required")
        run([tool, "-e", target_paths(environment)["kernel"], "-f", "-p", address], environment=environment)
        return
    try:
        action = simple[scope]
    except KeyError as error:
        raise ValueError(f"unknown workflow scope: {scope}") from error
    action(environment)


def main() -> int:
    """解析公开 scope 并返回 Make 可直接消费的退出码。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scope")
    arguments = parser.parse_args()
    try:
        dispatch(arguments.scope)
    except (OSError, RuntimeError, ValueError, subprocess.CalledProcessError) as error:
        print(f"workflow {arguments.scope} failed: {error}", file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError):
            return error.returncode or 1
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
