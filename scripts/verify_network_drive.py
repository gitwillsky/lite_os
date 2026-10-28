#!/usr/bin/env python3
"""在私有镜像以普通用户验证产品 SMB 映射：强制签名、强制加密、凭据重连与卸载。"""
from __future__ import annotations
import argparse
import json
import os
import selectors
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from build_target import target_from_environment
from ext4_image import find_debugfs, debugfs_path
from local_inputs import INPUTS
from qemu_gate import boot, guest_inittab, SHELL_PROMPT, ShellCommand
from sync_userland import synchronize
from verify_busybox import base_userland, build_musl_probe, BUSYBOX_KERNEL_ARGUMENTS
from verify_musl import cached_musl_paths, find_compiler, run

ROOT = Path(__file__).resolve().parents[1]

def start_server(policy: str, log) -> tuple[subprocess.Popen, dict]:
    """启动本次 gate 唯一 smbd supervisor 并读就绪记录；不把认证内容写入命令或日志。"""
    child = subprocess.Popen([sys.executable, str(ROOT / "scripts/samba_gate.py"), "--policy", policy], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=log, bufsize=0)
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            # 初次运行可能需要构建 Samba；构建日志与 JSON 共用 stdout，只有结构化就绪行可解锁启动。
            while selector.select(timeout=900):
                line = child.stdout.readline()
                if not line:
                    break
                try:
                    value = json.loads(line)
                    if isinstance(value, dict) and value.get("policy") == policy and "credentials" in value:
                        return child, value
                except (ValueError, TypeError):
                    pass
                log.write(line); log.flush()
        raise RuntimeError("Samba gate did not become ready; see samba.log")
    except BaseException:
        child.terminate(); child.wait(timeout=20)
        raise

def exercise(original: Path, policy: str, musl, probe: Path, output: Path) -> None:
    """只修改一次性 rootfs 副本；root 负责 fixture 装配，产品连接/文件操作均由 UID 65534 执行。"""
    with tempfile.TemporaryDirectory(prefix=f"network-{policy}-", dir=INPUTS / "samba/runs") as temporary, (output / f"{policy}-samba.log").open("wb") as log:
        work = Path(temporary)
        child, server = start_server(policy, log)
        try:
            image = work / "rootfs.img"
            shutil.copyfile(original, image)
            synchronize(image, base_userland(musl))
            inittab = work / "inittab"
            inittab.write_text(guest_inittab())
            # secret 只进一次性磁盘的 0600 文件；不经过串口命令、guest argv 或环境变量。
            credentials = Path(server["credentials"])
            script = ROOT / "scripts/fixtures/network-drive/check.sh"
            commands = [
                "rm /etc/inittab", f"write {debugfs_path(inittab)} /etc/inittab",
                f"write {debugfs_path(probe)} /var/tmp/network-drive-probe", "set_inode_field /var/tmp/network-drive-probe mode 0100755",
                f"write {debugfs_path(script)} /var/tmp/network-drive-check.sh", "set_inode_field /var/tmp/network-drive-check.sh mode 0100755",
                f"write {debugfs_path(credentials)} /var/tmp/smb-credentials", "set_inode_field /var/tmp/smb-credentials mode 0100600", "set_inode_field /var/tmp/smb-credentials uid 65534", "set_inode_field /var/tmp/smb-credentials gid 65534",
            ]
            batch = work / "fixture.debugfs"
            batch.write_text("\n".join(commands) + "\n")
            run([str(find_debugfs()), "-w", "-f", str(batch), str(image)], ROOT)
            # /tmp 是 rcS 创建的 tmpfs；HOME 在启动后设置所有权，不能依赖磁盘上的同名旧目录。
            command = f"mkdir -p /tmp/smb-user && chown 65534:65534 /tmp/smb-user && chmod 700 /tmp/smb-user && su -s /bin/sh nobody -c 'HOME=/tmp/smb-user XDG_CONFIG_HOME=/tmp/smb-user/.config /bin/sh /var/tmp/network-drive-check.sh {server['guest_source']} {policy} /var/tmp/smb-credentials'\n"
            text = boot(image, 2, (SHELL_PROMPT, "LITEOS_SMB_FILES_42", "LITEOS_SMB_42"), timeout_seconds=180, interactions=((SHELL_PROMPT, ShellCommand(command.encode())),), forbidden_markers=("LITEOS_SMB_FAILED", "Kernel panic", "panicked at"), kernel_arguments=BUSYBOX_KERNEL_ARGUMENTS)
            (output / f"{policy}-console.log").write_text(text)
        finally:
            child.terminate()
            try:
                child.wait(timeout=20)
            except subprocess.TimeoutExpired:
                child.kill(); child.wait()

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path)
    args = parser.parse_args()
    target = target_from_environment()
    image = args.image or ROOT / f"target/rootfs/{target.arch}.img"
    output = ROOT / "target/network-drive-runtime" / target.arch
    output.mkdir(parents=True, exist_ok=True)
    (INPUTS / "samba/runs").mkdir(parents=True, exist_ok=True)
    musl = cached_musl_paths(find_compiler())
    probe = build_musl_probe(musl, "network-drive")
    for policy in ("sign", "encrypt"):
        exercise(image, policy, musl, probe, output)
    print("network drive signing/encryption runtime gate passed")

if __name__ == "__main__":
    main()
