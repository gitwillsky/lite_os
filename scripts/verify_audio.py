#!/usr/bin/env python3
"""在 AArch64/HVF 上通过标准 ALSA consumer 验证内核混音与实际 WAV 输出。"""
from __future__ import annotations

import argparse
import json
import shutil
import sys
import tarfile
import tempfile
from pathlib import Path

from apk_cache import SOURCE, download
from audio_analysis import WavSignal, read_qemu_wav
from build_cache import (
    build_environment, cache_lock, fingerprint, manifest_matches, publish_directory,
    sha256, temporary_directory, write_manifest,
)
from build_target import acceleration_from_environment, target_from_environment
from ext4_image import recover_ext4_journal, find_debugfs
from qemu_gate import boot, guest_inittab
from toolchain.musl import cached_musl_paths, find_compiler, run

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target/audio-runtime"
FIXTURES = ROOT / "scripts/fixtures/audio-kernel"
# 只在私有 gate 镜像注入需要的上游 amixer、libasound 与配置；产品 rootfs 不继承测试工具。
PACKAGES = (
    ("alsa-lib-1.2.14-r0.apk", "da15717b4cc87913363bc53dff9c45d6341c84103b18352b208cd727aa8c3d70"),
    ("alsa-lib-dev-1.2.14-r0.apk", "ed3365b046f9ea708440a971e257779ed6b886748245be8b3311c366f07f89d4"),
    ("alsa-utils-1.2.14-r0.apk", "a68720a6a94bfc6bbae7cfea5a46b023a171a2d1b63365d463f1d7b23fc8f38e"),
)


def tools() -> tuple[Path, tuple[Path, ...]]:
    """返回已校验并解包的固定 ALSA 工具输入与 APK；错误时不发布源树。"""
    archives = tuple(download(name, digest) for name, digest in PACKAGES)
    payload = {"kind": "alsa-consumer-tools", "packages": dict(PACKAGES)}
    destination = SOURCE / "alsa-tools" / fingerprint(payload)
    with cache_lock(SOURCE / ".alsa-tools.lock"):
        if manifest_matches(destination, payload, ("usr/bin/amixer", "usr/lib/libasound.so.2", "usr/include/alsa/asoundlib.h")):
            return destination, archives
        temporary = temporary_directory(SOURCE / "alsa-tools", "extract")
        try:
            for archive in archives:
                with tarfile.open(archive, "r:gz") as package:
                    package.extractall(temporary, filter="data")
            write_manifest(temporary, payload)
            publish_directory(temporary, destination)
        finally:
            shutil.rmtree(temporary, ignore_errors=True)
    return destination, archives


def build_probe(musl, tool_root: Path, name: str) -> Path:
    """用固定 musl sysroot 构建真实 consumer；编译产物只进入 target。"""
    source = FIXTURES / f"{name}.c"
    payload = {
        "kind": "alsa-probe", "name": name, "source": sha256(source), "recipe": sha256(Path(__file__)),
        "sysroot": musl.sysroot_fingerprint, "driver": sha256(ROOT / "scripts/musl_clang.py"),
        "alsa": dict(PACKAGES),
    }
    destination = WORK / "probes" / fingerprint(payload)
    binary = destination / name
    if manifest_matches(destination, payload, (name,)):
        return binary
    generation = temporary_directory(WORK / "probes", "build")
    env = build_environment()
    env.update({
        "LITEOS_MUSL_CLANG": str(musl.compiler), "LITEOS_MUSL_LLD": str(musl.linker),
        "LITEOS_MUSL_COMPILER_RUNTIME": str(musl.compiler_runtime), "LITEOS_MUSL_SYSROOT": str(musl.install),
    })
    command = [sys.executable, ROOT / "scripts/musl_clang.py", source, "-D_GNU_SOURCE", "-std=gnu11", "-Wall", "-Wextra", "-Werror", "-fPIE", "-pie", "-pthread"]
    if name == "playback":
        command.extend(("-I", tool_root / "usr/include", "-L", tool_root / "usr/lib", "-l:libasound.so.2", "-lm"))
    command.extend(("-o", generation / name))
    try:
        run([str(argument) for argument in command], ROOT, env)
        write_manifest(generation, payload)
        publish_directory(generation, destination)
    finally:
        shutil.rmtree(generation, ignore_errors=True)
    return binary


def install(image: Path, tool_root: Path, probes: tuple[Path, ...], directory: Path) -> None:
    """将上游运行文件和验证探针注入私有镜像，保持产品基线不变。"""
    recover_ext4_journal(image)
    paths = [path for path in tool_root.rglob("*") if path.is_file() or path.is_symlink()]
    sources = []
    for path in paths:
        relative = path.relative_to(tool_root).as_posix()
        if relative == "usr/bin/amixer" or relative.startswith("usr/share/alsa/") or (relative.startswith("usr/lib/") and ".so" in path.name):
            sources.append((path, "/" + relative))
    sources.extend((probe, "/bin/alsa-" + probe.name) for probe in probes)
    script = directory / "audio.sh"
    script.write_text("#!/bin/sh\nset -eu\n/bin/alsa-control\n/usr/bin/amixer -c0 scontrols\n/usr/bin/amixer -c0 sset Master 100% unmute\necho ALSA_AMIXER_READY\n/bin/alsa-playback\necho ALSA_AUDIO_READY\n")
    sources.append((script, "/var/tmp/audio.sh"))
    inittab = directory / "inittab"
    inittab.write_text(guest_inittab("/bin/sh /var/tmp/audio.sh"))
    sources.append((inittab, "/etc/inittab"))
    directories = set()
    for _, destination in sources:
        parent = Path(destination).parent
        while str(parent) != "/":
            directories.add(str(parent)); parent = parent.parent
    commands = [f"mkdir {path}" for path in sorted(directories, key=lambda path: (path.count("/"), path))]
    for source, destination in sources:
        commands.append(f"rm {destination}")
        if source.is_symlink():
            commands.append(f"symlink {destination} {source.readlink()}")
        else:
            commands.extend((f"write {source} {destination}", f"set_inode_field {destination} mode 0100{source.stat().st_mode & 0o777:o}"))
    batch = directory / "install.debugfs"
    batch.write_text("\n".join(commands) + "\n")
    run([str(find_debugfs()), "-w", "-f", str(batch), str(image)], ROOT)


def analyze(signal: WavSignal) -> None:
    """从实际输出验证八路频率、声道增益、静音和削波；任一目标缺失即失败。"""
    onset = next((index for index, frame in enumerate(signal.frames) if max(map(abs, frame)) > 0.001), None)
    if onset is None:
        raise RuntimeError("audio output is silent")
    # 每阶段中间半秒避开设备队列和逐流启动边界；所有测试频率在该窗口内都有整数周期。
    for phase in range(6):
        first = onset + phase * 48_000 + 12_000
        window = signal.frames[first:first + 24_000]
        if len(window) != 24_000:
            raise RuntimeError(f"audio phase {phase} is truncated")
        segment = WavSignal(window, 48_000)
        if phase == 5:
            for channel in range(2):
                if any(not 0.995 < frame[channel] <= 1.0 for frame in window):
                    raise RuntimeError(f"audio overload did not hard clip channel {channel}")
            continue
        for channel in range(2):
            if phase == 4 and channel == 0:
                if segment.rms(channel) > 0.0001:
                    raise RuntimeError("muted left channel leaks audio")
                continue
            for stream in range(8):
                expected = (0.16 if stream == 0 else 0.0) if phase == 0 else 0.04
                if phase >= 2 and stream == 0 and channel == 0:
                    expected *= 0.25
                if phase >= 3:
                    expected *= 0.25 if channel == 0 else 0.5625
                measured = segment.tone_amplitude(440 + 110 * stream, channel=channel)
                if abs(measured - expected) > max(0.00015, expected * 0.03):
                    raise RuntimeError(f"audio phase={phase} stream={stream} channel={channel}: amplitude={measured:.6f}, expected={expected:.6f}")
    print("ALSA_WAV_READY phases=6 streams=8 channels=2 gain=mute=clip=ok")


def main() -> int:
    """执行 AArch64 实际设备门禁；RISC-V 只保留编译覆盖。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, default=ROOT / "target/rootfs/aarch64.img")
    args = parser.parse_args()
    if target_from_environment().arch != "aarch64" or acceleration_from_environment() != "hvf":
        print("audio runtime gate requires AArch64/HVF")
        return 0
    WORK.mkdir(parents=True, exist_ok=True)
    with cache_lock(WORK / ".build.lock"):
        musl = cached_musl_paths(find_compiler())
        tool_root, _ = tools()
        control = build_probe(musl, tool_root, "control")
        playback = build_probe(musl, tool_root, "playback")
    with tempfile.TemporaryDirectory(prefix="gate-", dir=WORK) as private:
        directory = Path(private)
        image = directory / "fs.img"
        shutil.copyfile(args.image, image)
        install(image, tool_root, (control, playback), directory)
        target = target_from_environment()
        identity = {
            "arch": target.arch, "accel": "hvf", "cpus": 4, "memory": "2G",
            "kernel_sha256": sha256(ROOT / target.kernel_boot_artifact()),
            "baseline_sha256": sha256(args.image), "private_image_sha256": sha256(image),
            "probes": {probe.name: sha256(probe) for probe in (control, playback)},
            "packages": dict(PACKAGES),
        }
        (WORK / "inputs.json").write_text(json.dumps(identity, indent=2) + "\n")
        print("audio input identity: " + json.dumps(identity, sort_keys=True))
        output = WORK / "output.wav"
        output.unlink(missing_ok=True)
        serial = boot(
            image, 4,
            ("ALSA_CONTROL_READY", "ALSA_AMIXER_READY", "ALSA_PLAYBACK_READY", "ALSA_AUDIO_READY"),
            timeout_seconds=90, memory="2G", audio_wav=output,
            forbidden_markers=("ALSA_PROBE_FAILED",), success_settle_seconds=0.25,
            # image 已是本 gate 独占的临时副本，无需再复制到系统临时目录。
            persistent_writes=True,
        )
        (WORK / "serial.log").write_text(serial)
        print("\n".join(line for line in serial.splitlines() if line.startswith("ALSA_")))
        analyze(read_qemu_wav(output))
    print("ALSA control, upstream amixer and eight-stream playback passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
