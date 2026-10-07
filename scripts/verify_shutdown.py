#!/usr/bin/env python3
"""在 make run 的 headless 设备装配上验证冷启动与 BusyBox shutdown，要求真实 QEMU 退出。"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import tempfile
import time

from build_cache import sha256
from build_target import target_from_environment
from host_topology import default_guest_cpu_count
from qemu_gate import ANSI, SHELL_PROMPT, send_interaction, terminate
from workflow import ROOT, _profile, _qemu_command


def verify(image: Path, command: str, smp: int) -> None:
    """在 private image 上注入 shutdown command，保存 identity/serial 并裁决 guest reset。

    Args:
        image: 已构建的只读 rootfs；每次冷启动使用私有副本。
        command: halt、poweroff 或 reboot；reboot 用 QEMU no-reboot 把 reset 转为 host 退出。
        smp: 本次完整 DTB CPU topology。

    Raises:
        RuntimeError: panic、headless GUI retry、命令未执行、QEMU 非零退出或超时。
    """
    target = target_from_environment()
    directory = ROOT / 'target/logs/shutdown'
    directory.mkdir(parents=True, exist_ok=True)
    name = f'{target.arch}-{smp}-{command}'
    with tempfile.TemporaryDirectory(prefix='liteos-shutdown-') as private:
        disk = Path(private) / 'rootfs.img'
        shutil.copyfile(image, disk)
        environment = dict(os.environ, QEMU_SMP=str(smp))
        argv = _qemu_command(disk, mode='run', memory='2G', environment=environment)
        argv.append('-no-reboot')
        profile = _profile(environment)
        artifacts = [image, ROOT / target.kernel_elf(profile), ROOT / target.kernel_boot_artifact(profile)]
        if target.requires_bootloader:
            artifacts.append(ROOT / 'bootloader/target' / target.kernel_triple / 'release/bootloader')
        (directory / f'{name}.json').write_text(json.dumps({
            'argv': argv, 'command': command, 'smp': smp, 'memory': '2G', 'profile': profile,
            'artifacts': {str(path): sha256(path) for path in artifacts},
        }, indent=2))
        process = subprocess.Popen(argv, cwd=ROOT, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        assert process.stdin is not None and process.stdout is not None
        output = bytearray()
        sent = False
        deadline = time.monotonic() + 60
        try:
            with (directory / f'{name}.log').open('wb') as log:
                while time.monotonic() < deadline:
                    ready, _, _ = select.select([process.stdout], [], [], .25)
                    if ready:
                        chunk = os.read(process.stdout.fileno(), 16384)
                        if not chunk:
                            break
                        output.extend(chunk)
                        log.write(chunk)
                        log.flush()
                    text = ANSI.sub('', output.decode(errors='replace'))
                    for forbidden in ('KERNEL PANIC', 'panicked at', 'retrying', 'desktop: display open:'):
                        if forbidden in text:
                            raise RuntimeError(f'{name}: reached {forbidden!r}')
                    if not sent and SHELL_PROMPT in text:
                        send_interaction(process.stdin, (command + '\n').encode())
                        sent = True
                else:
                    raise RuntimeError(f'{name}: shutdown timed out; command_sent={sent}')
            text = ANSI.sub('', output.decode(errors='replace'))
            required = ('The system is going down NOW!', 'Sent SIGTERM to all processes',
                        'Sent SIGKILL to all processes',
                        f'Requesting system {command}')
            if not sent or not all(marker in text for marker in required):
                raise RuntimeError(f'{name}: did not execute the complete shutdown sequence')
            code = process.wait(timeout=5)
            if code != 0:
                raise RuntimeError(f'{name}: QEMU returned {code}')
        finally:
            terminate(process)
        print(f'{target.arch} {smp}-CPU {command}: QEMU exited cleanly')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', type=Path, required=True)
    parser.add_argument('--smp', type=int, action='append')
    args = parser.parse_args()
    topologies = args.smp or sorted({1, default_guest_cpu_count()})
    if any(smp < 1 for smp in topologies):
        parser.error('--smp must be positive')
    for smp in topologies:
        for command in ('halt', 'poweroff', 'reboot'):
            verify(args.image.resolve(), command, smp)


if __name__ == '__main__':
    main()
