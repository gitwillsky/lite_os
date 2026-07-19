#!/usr/bin/env python3
"""在 guest 内完整验证固定 Alpine curl、SQLite 与 Git 应用竖切。"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import tempfile
from pathlib import Path

from apk_apps_cache import cached_application_apks
from build_cache import (
    fingerprint,
    publish_runtime_gate,
    runtime_gate_hit,
    runtime_gate_payload,
    sha256,
)
from ext2_image import find_debugfs, run_debugfs
from qemu_gate import boot, power_cut
from tls_gate import install_runtime_tls_identity, start_https_gate

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target" / "apk-apps-runtime"
FIXTURES = ROOT / "scripts" / "fixtures" / "apk-apps"
FORBIDDEN_MARKERS = (
    "unsupported syscall_id:",
    "panicked at",
    "[ERROR]",
    "Invalid argument",
)


def run(command: list[str], cwd: Path = ROOT) -> str:
    """执行 host assembly command，并保留失败输出。"""
    result = subprocess.run(
        command,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-80:])
        raise RuntimeError(f"command failed: {' '.join(command)}\n{tail}")
    return result.stdout.strip()


def apply_debugfs_script(image: Path, script: Path) -> None:
    """把一个已落盘的 debugfs transaction 应用于 disposable image。"""
    run([str(find_debugfs()), "-w", "-f", str(script), str(image)])


def inject_sysinit(
    image: Path,
    directory: Path,
    guest_script: Path,
    command: str,
    include_network_helper: bool = False,
) -> None:
    """把 guest 自验证脚本设为一次性 sysinit；host 只等待最终 marker。"""
    inittab = directory / f"{guest_script.stem}.inittab"
    inittab.write_text(f"::sysinit:{command}\n")
    transaction = directory / f"{guest_script.stem}.debugfs"
    commands = [
        f"write {guest_script} /run/{guest_script.name}",
        f"set_inode_field /run/{guest_script.name} mode 0100755",
    ]
    if include_network_helper:
        helper = FIXTURES / "network-up.sh"
        commands.extend(
            (
                f"write {helper} /run/apk-network-up.sh",
                "set_inode_field /run/apk-network-up.sh mode 0100755",
            )
        )
    commands.extend(("rm /etc/inittab", f"write {inittab} /etc/inittab"))
    transaction.write_text("\n".join(commands) + "\n")
    apply_debugfs_script(image, transaction)


def inject_sqlite_recovery_assets(image: Path, directory: Path) -> None:
    """在 mutation 前注入下一次 boot policy，避免 crash 后用 debugfs 绕过 journal replay。"""
    recovery = FIXTURES / "sqlite-recovery.sh"
    inittab = directory / "sqlite-recovery.inittab"
    inittab.write_text(f"::sysinit:/bin/sh /run/{recovery.name}\n")
    transaction = directory / "sqlite-recovery-assets.debugfs"
    transaction.write_text(
        f"write {recovery} /run/{recovery.name}\n"
        f"set_inode_field /run/{recovery.name} mode 0100755\n"
        f"write {inittab} /run/sqlite-recovery.inittab\n"
        f"write {ROOT / 'user' / 'base' / 'inittab'} /run/normal.inittab\n"
    )
    apply_debugfs_script(image, transaction)


def install_applications(base_image: Path, directory: Path) -> Path:
    """在 guest 中用真实 apk transaction 安装固定闭包，并缓存已安装镜像。"""
    apks = cached_application_apks()
    install_script = FIXTURES / "install.sh"
    payload = runtime_gate_payload(
        "apk-application-install",
        2,
        (
            base_image,
            ROOT / "target/riscv64gc-unknown-none-elf/debug/kernel",
            ROOT / "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
            install_script,
            Path(__file__).resolve(),
            ROOT / "scripts/apk_apps_cache.py",
            ROOT / "scripts/apk_cache.py",
            ROOT / "scripts/ext2_image.py",
            ROOT / "scripts/qemu_gate.py",
            *apks.archives,
        ),
    )
    identity = fingerprint(payload)
    installed = WORK / f"installed-{identity}.img"
    stamp = WORK / f"installed-{identity}.json"
    if runtime_gate_hit(stamp, payload, (installed,)):
        return installed

    WORK.mkdir(parents=True, exist_ok=True)
    temporary = directory / "installed.img"
    shutil.copyfile(base_image, temporary)
    normal_inittab = ROOT / "user" / "base" / "inittab"
    bootstrap_inittab = directory / "install.inittab"
    bootstrap_inittab.write_text("::sysinit:/bin/sh /run/verify-apk-install.sh\n")
    transaction = directory / "install.debugfs"
    commands = [
        "mkdir /run/apk-apps",
        f"write {install_script} /run/verify-apk-install.sh",
        "set_inode_field /run/verify-apk-install.sh mode 0100755",
        f"write {normal_inittab} /run/normal.inittab",
    ]
    commands.extend(f"write {archive} /run/apk-apps/{archive.name}" for archive in apks.archives)
    commands.extend(("rm /etc/inittab", f"write {bootstrap_inittab} /etc/inittab"))
    transaction.write_text("\n".join(commands) + "\n")
    apply_debugfs_script(temporary, transaction)
    boot(
        temporary,
        4,
        ("LITEOS_APK_APPLICATIONS_INSTALLED",),
        timeout_seconds=90,
        forbidden_markers=FORBIDDEN_MARKERS,
        persistent_writes=True,
    )
    installed_database = run_debugfs(temporary, "cat /lib/apk/db/installed")
    for package in ("curl", "sqlite", "git"):
        if f"P:{package}\n" not in installed_database:
            raise RuntimeError(f"guest APK transaction did not own {package}")
    if ".apk" in run_debugfs(temporary, "ls -l /run"):
        raise RuntimeError("installed application image retains APK transport archives")
    temporary.replace(installed)
    publish_runtime_gate(stamp, payload)
    return installed


def prepare_https_origin(directory: Path) -> tuple[Path, str]:
    """创建固定大 payload 与包含额外 remote branch 的 dumb-HTTP Git repository。"""
    origin = directory / "origin"
    origin.mkdir()
    payload = origin / "payload.bin"
    payload.write_bytes(bytes(range(256)) * 512)
    seed = directory / "git-seed"
    run(["git", "init", "-q", "-b", "main", str(seed)])
    run(["git", "config", "user.name", "LiteOS Gate"], seed)
    run(["git", "config", "user.email", "gate@liteos.invalid"], seed)
    (seed / "fixture.txt").write_text("git-over-https\n")
    run(["git", "add", "fixture.txt"], seed)
    run(["git", "commit", "-qm", "fixture"], seed)
    main_commit = run(["git", "rev-parse", "HEAD"], seed)
    run(["git", "checkout", "-qb", "gate-extra"], seed)
    (seed / "extra.txt").write_text("fetch-over-https\n")
    run(["git", "add", "extra.txt"], seed)
    run(["git", "commit", "-qm", "extra"], seed)
    run(["git", "checkout", "-q", "main"], seed)
    repository = origin / "repo.git"
    run(["git", "clone", "-q", "--bare", str(seed), str(repository)])
    run(["git", "--git-dir", str(repository), "update-server-info"])
    return origin, main_commit


def verify_network_applications(
    installed: Path,
    directory: Path,
    port: int,
    payload_hash: str,
    commit: str,
) -> None:
    """在一个 4-CPU guest 中验证 curl 与 Git 的完整 TLS/HTTP 竖切。"""
    image = directory / "network-apps.img"
    shutil.copyfile(installed, image)
    script = FIXTURES / "network-apps.sh"
    inject_sysinit(
        image,
        directory,
        script,
        f"/bin/sh /run/{script.name} {port} {payload_hash} {commit}",
        include_network_helper=True,
    )
    boot(
        image,
        4,
        (
            "LITEOS_APK_NETWORK_READY",
            "LITEOS_CURL_APPLICATION_READY",
            "LITEOS_GIT_LOCAL_READY",
            "LITEOS_GIT_REMOTE_READY",
            "LITEOS_GIT_APPLICATION_READY",
        ),
        timeout_seconds=90,
        forbidden_markers=FORBIDDEN_MARKERS,
    )


def verify_sqlite(installed: Path, directory: Path) -> None:
    persistent = directory / "sqlite.img"
    shutil.copyfile(installed, persistent)
    script = FIXTURES / "sqlite.sh"
    inject_sysinit(persistent, directory, script, f"/bin/sh /run/{script.name}")
    inject_sqlite_recovery_assets(persistent, directory)
    boot(
        persistent,
        4,
        ("LITEOS_SQLITE_APPLICATION_READY", "LITEOS_SQLITE_RECOVERY_READY"),
        timeout_seconds=90,
        forbidden_markers=FORBIDDEN_MARKERS,
        persistent_writes=True,
    )

    crashed = directory / "sqlite-crash.img"
    shutil.copyfile(persistent, crashed)
    mutation = (
        b"cp /run/sqlite-recovery.inittab /etc/inittab; sync; "
        b"echo LITEOS_SQLITE_MUTATION_ACTIVE; "
        b"while :; do sqlite3 /root/sqlite-gate.db 'BEGIN IMMEDIATE; "
        b"INSERT INTO records(value) VALUES(\"crash\"); COMMIT;' >/dev/null; done\n"
    )
    power_cut(crashed, 4, mutation, "LITEOS_SQLITE_MUTATION_ACTIVE", 0.15, 45)
    boot(
        crashed,
        4,
        ("LITEOS_SQLITE_RECOVERY_READY",),
        timeout_seconds=45,
        forbidden_markers=FORBIDDEN_MARKERS,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, default=ROOT / "fs.img")
    parser.add_argument("--output", type=Path, default=ROOT / "target" / "apk-apps.img")
    parser.add_argument("--build-only", action="store_true")
    args = parser.parse_args()
    base_image = args.image.resolve()
    if not base_image.is_file():
        raise RuntimeError(f"base rootfs image is missing: {base_image}")

    with tempfile.TemporaryDirectory(prefix="liteos-apk-apps-") as temporary:
        directory = Path(temporary)
        installed = install_applications(base_image, directory)
        if args.build_only:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(installed, args.output)
            print(f"APK application image passed: {args.output.resolve()}")
            return 0

        scripts = tuple(sorted(FIXTURES.glob("*.sh")))
        stamp = ROOT / "target" / "verify-gates" / "apk-apps.json"
        payload = runtime_gate_payload(
            "apk-applications-runtime",
            3,
            (
                installed,
                ROOT / "target/riscv64gc-unknown-none-elf/debug/kernel",
                ROOT / "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
                Path(__file__).resolve(),
                ROOT / "scripts/apk_apps_cache.py",
                ROOT / "scripts/ext2_image.py",
                ROOT / "scripts/https_gate.py",
                ROOT / "scripts/qemu_gate.py",
                ROOT / "scripts/tls_gate.py",
                *scripts,
            ),
        )
        if runtime_gate_hit(stamp, payload):
            print("APK curl/SQLite/Git runtime gate cache hit")
            return 0

        origin, commit = prepare_https_origin(directory)
        server, port, gate_ca = start_https_gate(directory, origin, range(18544, 18645))
        try:
            trusted = directory / "trusted.img"
            shutil.copyfile(installed, trusted)
            install_runtime_tls_identity(trusted, gate_ca, directory, find_debugfs())
            # 1. make verify 已并发运行四类 runtime gate；APK 内不得再并发 QEMU。
            # 2. curl/Git 共用同一 TLS/HTTP guest，避免重复冷启动；SQLite 独占 crash image。
            # 3. 全部成功后才发布统一 stamp，任一失败都不会留下部分成功状态。
            verify_network_applications(
                trusted,
                directory,
                port,
                sha256(origin / "payload.bin"),
                commit,
            )
        finally:
            server.terminate()
            server.wait(timeout=3)
        verify_sqlite(installed, directory)
        publish_runtime_gate(stamp, payload)
        print("APK curl/SQLite/Git application gates passed")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"APK application verification failed: {error}")
        raise SystemExit(1)
