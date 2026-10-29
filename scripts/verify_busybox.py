"""BusyBox consumer 的探针装配与 QEMU runtime 门禁。"""
from __future__ import annotations

import argparse
import io
import lzma
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from apk_rootfs import install_apk_crash_fixtures
from build_cache import (
    build_environment,
    build_jobs_override,
    cache_lock,
    fingerprint,
    generation_directory,
    manifest_matches,
    publish_generation,
    publish_runtime_gate,
    runtime_gate_hit,
    runtime_gate_payload,
    sha256,
    write_manifest,
)
from build_target import target_from_environment
from ext4_image import MKE2FS_CONFIG, find_debugfs, find_mke2fs
from openssl_cache import build_openssl
from pathlib import Path
from qemu_gate import (
    SHELL_PROMPT,
    ShellCommand,
    TerminalInput,
    boot,
    cpu_topology_markers,
    guest_inittab,
    power_cut,
)
from tls_gate import install_runtime_tls_identity, start_https_gate
from userland.busybox import BUSYBOX_VERSION, FORBIDDEN_BOOT_MARKERS, build_busybox, obtain_source
from userland.rust import verify_elf
from userland.image import create_published_image, target_runtime_artifacts
from userland.rust import build_rust_user_program
from toolchain.musl import MuslCachePaths, cached_musl_paths, find_compiler, run

ROOT = Path(__file__).resolve().parent.parent
TARGET = target_from_environment()
WORK = ROOT / "target" / "busybox-runtime" / TARGET.arch

# 主 BusyBox gate 以完整 kernel command line 冷启动：root/rootfstype/rootwait/console 由内核消费，
# LITEOS_BOOT_ENV 按 Linux 规则转交 init 环境并由 init 派生的 shell 继承，/proc/cmdline 原样回显。
# `root=LABEL=` 经超级块卷标解析根设备（不依赖设备枚举顺序）。
# `ro` 让根以只读挂载，由 rcS 的 `mount -o remount,rw /` 转为可写——同时覆盖该路径与 ext4 补做 orphan 回收。
BUSYBOX_KERNEL_ARGUMENTS = (
    "ro root=LABEL=LITEOS rootfstype=ext4 rootwait "
    f"console={'ttyAMA0' if TARGET.arch == 'aarch64' else 'ttyS0'} LITEOS_BOOT_ENV=42"
)


def start_http_gate() -> tuple[subprocess.Popen[bytes], int]:
    """在 QEMU slirp 稳定转发的非特权范围选择首个空闲 HTTP origin port。"""
    for port in range(18080, 18181):
        server = subprocess.Popen(
            [sys.executable, "-m", "http.server", str(port), "--bind", "127.0.0.1"],
            cwd=ROOT,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
        )
        try:
            server.wait(timeout=0.05)
        except subprocess.TimeoutExpired:
            return server, port
        if server.returncode == 0:
            raise RuntimeError("HTTP gate server exited without serving")
    raise RuntimeError("no free HTTP gate port in 18080..18180")


def install_archive_fixtures(image: Path, directory: Path) -> None:
    """向 disposable runtime image 注入上游格式 fixture，不把宿主解包结果当作 guest 验收。"""
    xz_fixture = directory / "phase53.xz"
    xz_fixture.write_bytes(lzma.compress(b"xz-phase-53\n", format=lzma.FORMAT_XZ))
    zip_fixture = directory / "phase53.zip"
    with zipfile.ZipFile(zip_fixture, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("nested/zip.txt", "zip-phase-53\n")
    traversal_fixture = directory / "phase53-traversal.tar"
    with tarfile.open(traversal_fixture, "w") as archive:
        payload = b"must-not-escape\n"
        entry = tarfile.TarInfo("../../phase53-escape")
        entry.size = len(payload)
        archive.addfile(entry, io.BytesIO(payload))
    commands = directory / "archive-fixtures.debugfs"
    commands.write_text(
        f"write {xz_fixture} /var/tmp/phase53.xz\n"
        f"write {zip_fixture} /var/tmp/phase53.zip\n"
        f"write {traversal_fixture} /var/tmp/phase53-traversal.tar\n"
    )
    run([str(find_debugfs()), "-w", "-f", str(commands), str(image)], ROOT)


def install_dhcp_gate_script(image: Path, directory: Path) -> None:
    """将 DHCP 断言放在 guest 内执行，避免后台日志打断 UART 长命令注入。"""
    fixture = directory / "dhcp-gate.sh"
    fixture.write_text(
        "#!/bin/sh\n"
        "set -e\n"
        "attempt=0\n"
        "while [ ! -s /etc/resolv.conf ]; do\n"
        "  attempt=$((attempt+1))\n"
        "  [ \"$attempt\" -lt 20 ] || exit 1\n"
        "  /bin/sleep 1\n"
        "done\n"
        "/bin/ifconfig eth0 | /bin/grep -q 'inet addr:10.0.2.15'\n"
        "/bin/route -n | /bin/grep -q '^0.0.0.0 .*10.0.2.2'\n"
        "/bin/grep -q 'lease of 10.0.2.15 obtained' /run/network-service.log\n"
        "echo LITEOS_DHCP_$((7*7+2))\n"
    )
    commands = directory / "dhcp-gate.debugfs"
    commands.write_text(
        f"write {fixture} /var/tmp/dhcp-gate.sh\n"
        "set_inode_field /var/tmp/dhcp-gate.sh mode 0100755\n"
    )
    run([str(find_debugfs()), "-w", "-f", str(commands), str(image)], ROOT)


def install_phase56_script(image: Path, directory: Path) -> None:
    """向 disposable runtime image 注入可移植脚本/安装工具链验收。"""
    fixture = directory / "phase56.sh"
    fixture.write_text(
        "#!/bin/sh\nset -e\n"
        "/bin/test 9223372036854775806 -gt 2147483647\n"
        "/bin/[ -d /tmp ]\n"
        "/bin/[[ alpha = alpha ']]'\n"
        "[ \"$(PHASE56=portable /bin/printenv PHASE56)\" = portable ]\n"
        "/bin/yes phase56 | /bin/head -n 3 >/run/yes56\n"
        "[ \"$(/bin/wc -l </run/yes56)\" -eq 3 ]\n"
        "echo LITEOS_SCRIPT_PRIMITIVES_$((7*8))\n"
        "/bin/rm -f /run/tmp56.* /tmp/phase56.*\n"
        "workers=''; i=0\n"
        "while [ $i -lt 8 ]; do (/bin/mktemp /tmp/phase56.XXXXXX >/run/tmp56.$i) & workers=\"$workers $!\"; i=$((i+1)); done\n"
        "ok=1; for worker in $workers; do wait $worker || ok=0; done\n"
        "/bin/cat /run/tmp56.[0-7] >/run/tmp56.list\n"
        "lines=$(/bin/wc -l </run/tmp56.list); unique=$(/bin/sort -u /run/tmp56.list | /bin/wc -l)\n"
        "for path in $(/bin/cat /run/tmp56.list); do [ \"$(/bin/stat -c %a $path)\" = 600 ] || ok=0; done\n"
        "directory=$(/bin/mktemp -d /tmp/phase56-dir.XXXXXX)\n"
        "[ \"$ok:$lines:$unique:$(/bin/stat -c %a $directory):$(/bin/stat -c %a /tmp)\" = 1:8:8:700:1777 ]\n"
        "echo LITEOS_MKTEMP_$((7*8))\n"
        "/bin/rm -rf /phase56 /stage56; /bin/mkdir /phase56\n"
        "/bin/printf 'configure-phase-56\\n' >/phase56/source\n"
        "/bin/ln -s source /phase56/link\n"
        "source_time=$(/bin/stat -c %Y /phase56/source)\n"
        "/bin/install -D -p -o 1000 -g 1000 -m 0750 /phase56/source /stage56/usr/bin/demo\n"
        "/bin/install -d -o 1000 -g 1000 -m 0750 /stage56/var/lib/demo\n"
        "if /bin/install /phase56/missing /stage56/bad 2>/dev/null; then exit 1; fi\n"
        "fields=$(/bin/stat -c '%a:%u:%g:%s:%Y' /stage56/usr/bin/demo)\n"
        "[ \"$fields\" = \"750:1000:1000:19:$source_time\" ]\n"
        "[ \"$(/bin/stat -c %F /phase56/link)\" = 'symbolic link' ]\n"
        "[ \"$(/bin/stat -Lc %F /phase56/link)\" = 'regular file' ]\n"
        "/bin/test /phase56/source -ef /phase56/link\n"
        "[ \"$(/bin/stat -c '%a:%u:%g' /stage56/var/lib/demo)\" = 750:1000:1000 ]\n"
        "[ ! -e /stage56/bad ]\n"
        "[ \"$(/bin/stat -f -c %b /)\" -gt 0 ]\n"
        "echo LITEOS_INSTALL_$((7*8))\n"
        "cd /stage56/usr/bin\n"
        "/bin/md5sum demo >/run/demo.md5; /bin/sha1sum demo >/run/demo.sha1\n"
        "/bin/sha256sum demo >/run/demo.sha256; /bin/sha512sum demo >/run/demo.sha512\n"
        "/bin/md5sum -c /run/demo.md5 >/dev/null; /bin/sha1sum -c /run/demo.sha1 >/dev/null\n"
        "/bin/sha256sum -c /run/demo.sha256 >/dev/null; /bin/sha512sum -c /run/demo.sha512 >/dev/null\n"
        "set -- $(/bin/cksum demo); cd /; [ \"$2\" -eq 19 ]\n"
        "echo LITEOS_CHECKSUM_$((7*8))\n"
        "[ \"$(/bin/whoami)\" = root ]; [ \"$(/bin/groups)\" = root ]\n"
        "[ \"$(PHASE56=identity /bin/printenv PHASE56)\" = identity ]\n"
        "[ \"$(/bin/stat -c %a /root)\" = 700 ]\n"
        "echo LITEOS_IDENTITY_$((7*8))\n"
    )
    commands = directory / "phase56-debugfs.commands"
    commands.write_text(
        f"write {fixture} /var/tmp/phase56.sh\n"
        "set_inode_field /var/tmp/phase56.sh mode 0100755\n"
    )
    run([str(find_debugfs()), "-w", "-f", str(commands), str(image)], ROOT)


def install_phase57_script(image: Path, directory: Path) -> None:
    """向 disposable runtime image 注入 opened-file identity 与标准 fd alias 验收。"""
    fixture = directory / "phase57.sh"
    fixture.write_text(
        "#!/bin/sh\nset -e\n"
        "[ \"$(/bin/tty)\" = /dev/console ]\n"
        "[ \"$(/bin/readlink /proc/self/fd/0)\" = /dev/console ]\n"
        "[ \"$(/bin/readlink /dev/fd)\" = /proc/self/fd ]\n"
        "[ \"$(/bin/readlink /dev/stdin)\" = /proc/self/fd/0 ]\n"
        "/bin/ls /proc/self/fd | /bin/grep -qx 0\n"
        "/bin/printf pipe57 | /bin/sh -c \"/bin/readlink /proc/self/fd/0 | /bin/grep -Eq '^pipe:\\[[0-9]+\\]$'\"\n"
        "echo LITEOS_TTY_$((3*19))\n"
        "/bin/rm -f /tmp/opened57 /tmp/renamed57 /tmp/alias57 /tmp/fresh57\n"
        "exec 3>/tmp/opened57\n"
        "[ \"$(/bin/readlink /proc/self/fd/3)\" = /tmp/opened57 ]\n"
        "/bin/printf before >/dev/fd/3\n"
        "[ \"$(/bin/cat /tmp/opened57)\" = before ]\n"
        "/bin/mv /tmp/opened57 /tmp/renamed57\n"
        "[ \"$(/bin/readlink /proc/self/fd/3)\" = /tmp/renamed57 ]\n"
        "/bin/ln /tmp/renamed57 /tmp/alias57\n"
        "exec 4>>/tmp/alias57\n"
        "[ \"$(/bin/readlink /proc/self/fd/4)\" = /tmp/alias57 ]\n"
        "/bin/rm /tmp/renamed57\n"
        "[ \"$(/bin/readlink /proc/self/fd/3)\" = '/tmp/renamed57 (deleted)' ]\n"
        "[ \"$(/bin/readlink /proc/self/fd/4)\" = /tmp/alias57 ]\n"
        "/bin/printf magic >>/dev/fd/3\n"
        "[ \"$(/bin/cat /tmp/alias57)\" = beforemagic ]\n"
        "/bin/printf after >&3\n"
        "[ \"$(/bin/cat /tmp/alias57)\" = afteremagic ]\n"
        "echo LITEOS_OPENED_RENAME_$((3*19))\n"
        "exec 5>/tmp/fresh57; [ \"$(/bin/readlink /proc/self/fd/5)\" = /tmp/fresh57 ]\n"
        "exec 5>&-\n"
        "if /bin/readlink /proc/self/fd/5 >/dev/null 2>&1; then exit 1; fi\n"
        "/bin/printf stdout57 >/dev/stdout\n"
        "echo LITEOS_OPENED_FD_$((3*19))\n"
    )
    commands = directory / "phase57-debugfs.commands"
    commands.write_text(
        f"write {fixture} /var/tmp/phase57.sh\n"
        "set_inode_field /var/tmp/phase57.sh mode 0100755\n"
    )
    run([str(find_debugfs()), "-w", "-f", str(commands), str(image)], ROOT)


def install_phase55_script(image: Path, directory: Path) -> None:
    """向 disposable runtime image 注入进程发现与生命周期验收。"""
    fixture = directory / "phase55.sh"
    fixture.write_text(
        "#!/bin/sh\nset -e\n"
        "/bin/sleep 30 & p1=$!; /bin/sleep 30 & p2=$!\n"
        "/bin/grep -q '^Name:[[:space:]]*sleep$' /proc/$p1/status\n"
        "[ \"$(/bin/cat /proc/$p1/comm)\" = sleep ]\n"
        "/bin/tr '\\000' ' ' </proc/$p1/cmdline | /bin/grep -q '/bin/sleep 30 '\n"
        "/bin/readlink /proc/self | /bin/grep -Eq '^[0-9]+$'\n"
        "case \" $(/bin/pidof sleep) \" in *\" $p1 \"*) true;; *) false;; esac\n"
        "/bin/pgrep sleep | /bin/grep -qx \"$p2\"\n"
        "/bin/pgrep -f '/bin/sleep 30' | /bin/grep -qx \"$p1\"\n"
        "echo LITEOS_PROCFS_$((5*11))\n"
        "/bin/pkill -TERM -P $$ sleep\n"
        "set +e; wait $p1; s1=$?; wait $p2; s2=$?; set -e\n"
        "/bin/tail -f /dev/null & tail55=$!\n"
        "while [ \"$(/bin/cat /proc/$tail55/comm 2>/dev/null)\" != tail ]; do :; done\n"
        "/bin/killall tail\n"
        "set +e; wait $tail55; tail_status=$?; set -e\n"
        "[ \"$s1:$s2:$tail_status\" = 143:143:143 ]; [ ! -d /proc/$p1 ]\n"
        "echo LITEOS_PROCESS_SIGNAL_$((5*11))\n"
        "set +e; /bin/timeout 1 /bin/sleep 30; timed=$?; set -e\n"
        "/bin/nohup /bin/sleep 2 >/dev/null 2>&1 & nohup55=$!\n"
        "while [ \"$(/bin/cat /proc/$nohup55/comm 2>/dev/null)\" != sleep ]; do :; done\n"
        "/bin/kill -HUP $nohup55; wait $nohup55; nohup_status=$?\n"
        "[ \"$timed:$nohup_status\" = 143:0 ]\n"
        "echo LITEOS_PROCESS_LIFECYCLE_$((5*11))\n"
        "/bin/rm -f /watch55\n"
        "/bin/watch -n 1 -t '/bin/echo refresh' >/watch55 & watch55=$!\n"
        "/bin/sleep 2; /bin/kill -TERM $watch55\n"
        "set +e; wait $watch55; watch_status=$?; set -e\n"
        "refreshes=$(/bin/grep -c refresh /watch55)\n"
        "[ \"$watch_status\" -eq 143 ]; [ \"$refreshes\" -ge 2 ]\n"
        "/bin/stty -a | /bin/grep -q echo\n"
        "echo LITEOS_WATCH_$((5*11))\n"
        "echo LITEOS_PROCESS_TOOLS_$((5*11))\n"
    )
    commands = directory / "phase55-debugfs.commands"
    commands.write_text(
        f"write {fixture} /var/tmp/phase55.sh\n"
        "set_inode_field /var/tmp/phase55.sh mode 0100755\n"
    )
    run([str(find_debugfs()), "-w", "-f", str(commands), str(image)], ROOT)


def install_guest_gate_init(
    image: Path, directory: Path, script: str, gate_name: str
) -> None:
    """让 disposable image 由 BusyBox init 直接执行唯一 guest 自检脚本。"""
    fixture = directory / f"{gate_name}-inittab"
    fixture.write_text(guest_inittab(f"/bin/sh {script}"))
    commands = directory / f"{gate_name}-inittab.debugfs"
    commands.write_text(
        "rm /etc/inittab\n"
        f"write {fixture} /etc/inittab\n"
    )
    run([str(find_debugfs()), "-w", "-f", str(commands), str(image)], ROOT)


def install_kernel_stack_churn_gate(image: Path, directory: Path) -> None:
    """向 disposable image 注入双 CPU kernel-stack retirement 回归入口。

    Args:
        image: 当前未被 QEMU 使用的 gate-private ext4 镜像。
        directory: host 临时命令与 inittab fixture 的唯一目录。

    Returns:
        None；脚本与 sysinit 入口完整写入镜像后返回。

    Raises:
        subprocess.CalledProcessError: debugfs 无法更新 private image。
    """
    fixture = ROOT / "scripts/fixtures/kernel-stack-churn.sh"
    commands = directory / "kernel-stack-churn.debugfs"
    commands.write_text(
        f"write {fixture} /var/tmp/kernel-stack-churn.sh\n"
        "set_inode_field /var/tmp/kernel-stack-churn.sh mode 0100755\n"
    )
    run([str(find_debugfs()), "-w", "-f", str(commands), str(image)], ROOT)
    install_guest_gate_init(
        image,
        directory,
        "/var/tmp/kernel-stack-churn.sh",
        "kernel-stack-churn",
    )


def build_dynamic_probe(musl: MuslCachePaths) -> tuple[Path, Path]:
    payload = {
        "kind": "dynamic-loader-probe",
        "recipe_version": 2,
        "arch": TARGET.arch,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "driver_sha256": sha256(ROOT / "scripts/musl_clang.py"),
        "main_sha256": sha256(ROOT / "scripts/fixtures/musl/dynamic-smoke.c"),
        "spawn_sha256": sha256(ROOT / "scripts/fixtures/musl/process-spawn.c"),
        "library_sha256": sha256(ROOT / "scripts/fixtures/musl/dynamic-smoke-lib.c"),
    }
    entry = WORK / "dynamic-probes" / fingerprint(payload)
    if manifest_matches(entry, payload, ("dynamic-smoke", "libliteos-smoke.so")):
        return entry / "dynamic-smoke", entry / "libliteos-smoke.so"
    generation = generation_directory(WORK / "dynamic-probe-generations", fingerprint(payload))
    env = build_environment()
    env.update({
        "LITEOS_MUSL_CLANG": str(musl.compiler),
        "LITEOS_MUSL_LLD": str(musl.linker),
        "LITEOS_MUSL_COMPILER_RUNTIME": str(musl.compiler_runtime),
        "LITEOS_MUSL_SYSROOT": str(musl.install),
    })
    published = False
    try:
        run(
            [
                str(musl.compiler),
                f"--target={TARGET.linux_triple}",
                f"--ld-path={musl.linker}",
                "-nostdlib",
                "-shared",
                "-fPIC",
                "-Wl,-z,relro,-z,now,-z,noexecstack",
                str(ROOT / "scripts/fixtures/musl/dynamic-smoke-lib.c"),
                "-o",
                str(generation / "libliteos-smoke.so"),
            ],
            ROOT,
            env,
        )
        run(
            [
                sys.executable,
                str(ROOT / "scripts/musl_clang.py"),
                str(ROOT / "scripts/fixtures/musl/dynamic-smoke.c"),
                str(ROOT / "scripts/fixtures/musl/process-spawn.c"),
                "-fPIE",
                "-pie",
                "-ldl",
                "-o",
                str(generation / "dynamic-smoke"),
            ],
            ROOT,
            env,
        )
        write_manifest(generation, payload)
        publish_generation(generation, entry)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    return entry / "dynamic-smoke", entry / "libliteos-smoke.so"

SCRATCH_DISK_BYTES = 16 * 1024 * 1024

# MBR 分区（512 字节扇区）：p1 起点 1 MiB、长 4 MiB 用于裸 I/O；p2 起点 5 MiB、长 10 MiB 放 ext4。
SCRATCH_PARTITIONS = ((2048, 8192), (10240, 20480))


def create_scratch_disk(path: Path) -> None:
    """创建裸块设备 gate 的第二块盘：MBR 两个 4 KiB 对齐分区，第二个分区内是固定 profile 的 ext4。"""
    with path.open("wb") as disk:
        # 首块是已知标记；MBR 表项与签名在 446..512，与标记不重叠。
        header = bytearray(b"LITEOS-SCRATCH-DISK-HEADER".ljust(4096, b"\0"))
        for slot, (start, sectors) in enumerate(SCRATCH_PARTITIONS):
            entry = 446 + slot * 16
            header[entry + 4] = 0x83
            header[entry + 8 : entry + 12] = start.to_bytes(4, "little")
            header[entry + 12 : entry + 16] = sectors.to_bytes(4, "little")
        header[510:512] = b"\x55\xaa"
        disk.write(header)
        disk.truncate(SCRATCH_DISK_BYTES)
    start, sectors = SCRATCH_PARTITIONS[1]
    config = path.with_suffix(".conf")
    config.write_text(MKE2FS_CONFIG)
    try:
        subprocess.run(
            [
                str(find_mke2fs()),
                "-t", "ext4",
                "-J", "size=4",
                "-E", f"offset={start * 512}",
                str(path),
                f"{sectors * 512 // 1024}k",  # 单位是 KiB；不带单位时 mke2fs 按 1 KiB 解释块数
            ],
            check=True,
            capture_output=True,
            env={**os.environ, "MKE2FS_CONFIG": str(config)},
        )
    finally:
        config.unlink(missing_ok=True)


def build_musl_probe(musl: MuslCachePaths, name: str) -> Path:
    """构建只用于 runtime gate 的单文件 musl probe（`scripts/fixtures/musl/<name>.c`）。"""
    source = ROOT / "scripts/fixtures/musl" / f"{name}.c"
    payload = {
        "kind": f"{name}-probe",
        "recipe_version": 1,
        "arch": TARGET.arch,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "driver_sha256": sha256(ROOT / "scripts/musl_clang.py"),
        "source_sha256": sha256(source),
    }
    binary = f"{name}-probe"
    entry = WORK / f"{name}-probes" / fingerprint(payload)
    if manifest_matches(entry, payload, (binary,)):
        return entry / binary
    generation = generation_directory(WORK / f"{name}-probe-generations", fingerprint(payload))
    env = build_environment()
    env.update({
        "LITEOS_MUSL_CLANG": str(musl.compiler),
        "LITEOS_MUSL_LLD": str(musl.linker),
        "LITEOS_MUSL_COMPILER_RUNTIME": str(musl.compiler_runtime),
        "LITEOS_MUSL_SYSROOT": str(musl.install),
    })
    published = False
    try:
        run(
            [
                sys.executable,
                str(ROOT / "scripts/musl_clang.py"),
                str(source),
                "-std=gnu11",
                "-D_GNU_SOURCE",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-fPIE",
                "-pie",
                "-pthread",
                "-o",
                str(generation / binary),
            ],
            ROOT,
            env,
        )
        write_manifest(generation, payload)
        publish_generation(generation, entry)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    return entry / binary


def install_musl_probes(image: Path, probes: dict[str, Path], workspace: Path) -> None:
    """只向一次性 runtime-gate 镜像注入 verification-only probe（不进入产品 rootfs）。"""
    commands = workspace / "musl-probes.debugfs"
    commands.write_text(
        "".join(
            f"write {path} /var/tmp/{name}\nset_inode_field /var/tmp/{name} mode 0100755\n"
            for name, path in probes.items()
        )
    )
    run([str(find_debugfs()), "-w", "-f", str(commands), str(image)], ROOT)


def install_runtime_execution_fixtures(
    image: Path,
    dynamic_probe: Path,
    dynamic_library: Path,
    workspace: Path,
) -> None:
    """只向一次性 runtime-gate 镜像注入 ABI 与 shebang consumer。

    Args:
        image: 当前 runtime gate 独占的可写镜像。
        dynamic_probe: 已校验的动态 musl consumer executable。
        dynamic_library: consumer 加载的已校验 shared object。
        workspace: script 与 debugfs command file 的 gate-private 目录。

    Returns:
        None。返回时 supplied image 包含三个 verification-only 文件。

    Raises:
        OSError: 写入 gate-private fixture 文件失败。
        subprocess.CalledProcessError: debugfs 无法更新 private image。
    """
    executable_script = workspace / "liteos-script"
    executable_script.write_text(
        "#!/bin/sh -e\n"
        "[ \"$0\" = /bin/liteos-script ]\n"
        "[ \"$1:$2\" = 'alpha beta:omega' ]\n"
        "echo LITEOS_SCRIPT_EXEC_$((6*7))\n"
    )
    commands = workspace / "execution-fixtures.debugfs"
    commands.write_text(
        f"write {dynamic_library} /usr/lib/libliteos-smoke.so\n"
        f"write {dynamic_probe} /bin/dynamic-smoke\n"
        "set_inode_field /bin/dynamic-smoke mode 0100755\n"
        f"write {executable_script} /bin/liteos-script\n"
        "set_inode_field /bin/liteos-script mode 0100755\n"
    )
    run([str(find_debugfs()), "-w", "-f", str(commands), str(image)], ROOT)


def build_drm_flip_probe(musl: MuslCachePaths) -> Path:
    """构建内核显示/GPU 门禁使用的 DRM page-flip 探针；它不进入产品 rootfs。"""
    return build_rust_user_program(
        musl,
        "linux-uapi",
        "drm-flip-probe",
        "drm-flip-probe",
        1,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--build-only",
        action="store_true",
        help="只构建 rootfs，不执行 runtime gates；cache miss 时仍启动 target guest 装配 APK 数据库",
    )
    parser.add_argument(
        "--image",
        type=Path,
        default=WORK / "fs.img",
        help="rootfs 输出路径",
    )
    parser.add_argument(
        "--rebuild",
        action="store_true",
        help="忽略当前 fingerprint 的 BusyBox ELF 命中并重新构建",
    )
    args = parser.parse_args()
    runtime_directory: tempfile.TemporaryDirectory[str] | None = None
    http_server: subprocess.Popen[bytes] | None = None
    https_server: subprocess.Popen[bytes] | None = None
    try:
        WORK.mkdir(parents=True, exist_ok=True)
        jobs_override = build_jobs_override()
        compiler = find_compiler()
        with cache_lock(WORK / ".build.lock"):
            source = obtain_source()
            binary = build_busybox(source, compiler, jobs_override, args.rebuild)
            verify_elf(binary, compiler)
            musl = cached_musl_paths(compiler)
            openssl = build_openssl(musl, jobs_override, args.rebuild)
        if args.build_only:
            image = create_published_image(binary, musl, openssl, args.image.resolve())
            print(f"BusyBox {BUSYBOX_VERSION} rootfs build passed: {image}")
            return 0
        dynamic_probe, dynamic_library = build_dynamic_probe(musl)
        # 启动前的成功指纹必须包含实际执行的每个 probe，不能先命中旧 stamp 再漏掉新 consumer。
        probes = {
            "memory-file-probe": build_musl_probe(musl, "memory-file"),
            "special-file-probe": build_musl_probe(musl, "special-file"),
            "fs-stress-probe": build_musl_probe(musl, "fs-stress"),
            "inotify-probe": build_musl_probe(musl, "inotify"),
            "fuse-probe": build_musl_probe(musl, "fuse"),
        }
        stamp = ROOT / f"target/verify-gates/busybox-{TARGET.arch}.json"
        payload = runtime_gate_payload(
            "busybox-runtime",
            13,
            (
                *target_runtime_artifacts(),
                binary,
                musl.install / "usr/lib/libc.so",
                dynamic_probe,
                dynamic_library,
                *probes.values(),
                openssl.binary,
                ROOT / "user/base/passwd",
        ROOT / "user/base/shadow",
                ROOT / "user/base/group",
                ROOT / "user/base/inittab",
                ROOT / "user/base/profile",
                ROOT / "user/Cargo.toml",
                ROOT / "user/Cargo.lock",
                ROOT / "user/base/network-service",
                ROOT / "user/base/shutdown",
                ROOT / "user/base/udhcpc.script",
                ROOT / "user/base/interfaces",
        ROOT / "user/base/interfaces",
                ROOT / "create_fs.py",
                Path(__file__).resolve(),
                *(ROOT / "scripts/userland" / name for name in ("busybox.py", "rust.py", "artifacts.py", "image.py")),
                ROOT / "scripts/https_gate.py",
                ROOT / "scripts/tls_gate.py",
                ROOT / "scripts/ext4_image.py",
                ROOT / "scripts/apk_cache.py",
                ROOT / "scripts/apk_package.py",
                ROOT / "scripts/apk_rootfs.py",
                ROOT / "scripts/openssl_cache.py",
                ROOT / "scripts/qemu_gate.py",
                ROOT / "scripts/fixtures/kernel-stack-churn.sh",
            ),
        )
        image = args.image.resolve()
        if runtime_gate_hit(stamp, payload, (image,)):
            print(f"BusyBox {BUSYBOX_VERSION} init+ash verification cache hit")
            return 0
        image = create_published_image(binary, musl, openssl, image)
        runtime_directory = tempfile.TemporaryDirectory(prefix="liteos-busybox-gate-")
        runtime_path = Path(runtime_directory.name)
        runtime_image = runtime_path / "fs.img"
        shutil.copyfile(image, runtime_image)
        install_runtime_execution_fixtures(
            runtime_image,
            dynamic_probe,
            dynamic_library,
            runtime_path,
        )
        install_musl_probes(
            runtime_image,
            probes,
            runtime_path,
        )
        scratch_disk = runtime_path / "scratch.img"
        create_scratch_disk(scratch_disk)
        http_server, http_port = start_http_gate()
        https_server, https_port, gate_ca = start_https_gate(runtime_path)
        install_runtime_tls_identity(runtime_image, gate_ca, runtime_path, find_debugfs())
        install_archive_fixtures(runtime_image, runtime_path)
        install_dhcp_gate_script(runtime_image, runtime_path)
        install_phase55_script(runtime_image, runtime_path)
        install_phase56_script(runtime_image, runtime_path)
        install_phase57_script(runtime_image, runtime_path)
        phase55_image = runtime_path / "phase55.img"
        phase56_image = runtime_path / "phase56.img"
        phase57_image = runtime_path / "phase57.img"
        kernel_stack_churn_image = runtime_path / "kernel-stack-churn.img"
        shutil.copyfile(runtime_image, phase55_image)
        shutil.copyfile(runtime_image, phase56_image)
        shutil.copyfile(runtime_image, phase57_image)
        shutil.copyfile(runtime_image, kernel_stack_churn_image)
        install_guest_gate_init(phase55_image, runtime_path, "/var/tmp/phase55.sh", "phase55")
        install_guest_gate_init(phase56_image, runtime_path, "/var/tmp/phase56.sh", "phase56")
        install_guest_gate_init(phase57_image, runtime_path, "/var/tmp/phase57.sh", "phase57")
        install_kernel_stack_churn_gate(kernel_stack_churn_image, runtime_path)
        # 该组合 gate 串行覆盖 50+ 次 UART interaction、TLS、archive、editor、并发 VFS 与
        # job-control；90 秒只是不受 host 调度影响的 liveness bound，不是性能阈值。热路径
        # 性能由 release instruction/count gates 独立约束，不能从这里删 marker 或 workload。
        boot(
            runtime_image,
            1,
            (
                *cpu_topology_markers(1),
                "init started: BusyBox v1.37.0",
                "LITEOS_BUSYBOX_SHELL_42",
                "LITEOS_BUSYBOX_UNICODE_42",
                "LITEOS_DHCP_51",
                "LITEOS_DHCP_SINGLE_51",
                "LITEOS_DHCP_RESPAWN_51",
                "LITEOS_PING_52",
                "LITEOS_DNS_51",
                "LITEOS_HTTP_51",
                "LITEOS_TLS_REJECT_52",
                "LITEOS_HTTPS_52",
                "LITEOS_TAR_53",
                "LITEOS_COMPRESSION_53",
                "LITEOS_TOOLS_53",
                "LITEOS_ARCHIVE_53",
                "LITEOS_VI_54",
                "LITEOS_LESS_54",
                "LITEOS_TEXT_DIAG_54",
                "LITEOS_LS_42",
                "LITEOS_NULL_42",
                "LITEOS_ZERO_4",
                "LITEOS_TTYDEV_42",
                "LITEOS_CONSOLEDEV_42",
                "LITEOS_DEVCWD_42",
                "LITEOS_TEXT_42",
                "LITEOS_FILTERS_42",
                "LITEOS_FIND_42",
                "LITEOS_MATH_42",
                "LITEOS_TOOLS_42",
                "LITEOS_OBSERVABILITY_42",
                "LITEOS_FILESYSTEM_CAPACITY_42",
                "LITEOS_MOUNT_44",
                "LITEOS_CMDLINE_45",
                "LITEOS_TMPFS_46",
                "LITEOS_TMPFS_47",
                "LITEOS_TMPFS_48",
                "LITEOS_TMPFS_49",
                "LITEOS_TMPFS_DEEP_52",
                "LITEOS_UNLINK_RELEASE_50",
                "LITEOS_UNLINK_RELEASE_51",
                "LITEOS_CACHE_RETIREMENT_61",
                "LITEOS_MOUNTFLAGS_53",
                "LITEOS_MOUNTFLAGS_54",
                "LITEOS_MOUNTFLAGS_55",
                "LITEOS_KMSG_56",
                "LITEOS_KMSG_57",
                "LITEOS_BLOCK_58",
                "LITEOS_BLOCK_59",
                "LITEOS_BLOCK_60",
                "LITEOS_PARTITION_61",
                "LITEOS_BLOCKSYS_62",
                "LITEOS_MEMFILE_42",
                "LITEOS_SPECIAL_42",
                "LITEOS_INOTIFY_42",
                "LITEOS_LINKS_43",
                "LITEOS_NAMESPACE_CONCURRENCY_43",
                "LITEOS_BUSYBOX_CREDENTIALS_44",
                "LITEOS_SYSTEM_IDENTITY_42",
                "LITEOS_WALLCLOCK_42",
                "LITEOS_EXEC_RECLAIM_42",
                "LITEOS_TOP_42",
                "LITEOS_READLINK_42",
                "LITEOS_DLOPEN_42",
                "LITEOS_CREDENTIALS_44",
                "LITEOS_SCRIPT_EXEC_42",
                "LITEOS_KILL_GROUP_42",
                "LITEOS_ARCHIVE_42",
                "LITEOS_PIPE_42",
                "LITEOS_REDIR_42",
                "LITEOS_BG_42",
                "LITEOS_PERSIST_WRITTEN_42",
                "LITEOS_JOBS_STOPPED_42",
                "LITEOS_BG_CONTINUED_42",
                "LITEOS_FG_CTRL_C_42",
                "LITEOS_TTY_CTRL_C_42",
                "LITEOS_BG_READ_STOPPED_42",
                "LITEOS_BG_READ_OK_42",
                "LITEOS_BG_READ_IGNORED_EIO_42",
                "LITEOS_BG_WRITE_STOPPED_42",
                "LITEOS_BG_WRITE_OK_42",
                "LITEOS_BG_WRITE_IGNORED_42",
                "LITEOS_INIT_SURVIVED_42",
                "LITEOS_ORPHAN_HUP_42",
                "LITEOS_ORPHAN_CONT_42",
                "LITEOS_SESSION_HUP_42",
                "LITEOS_ROOT_REMOUNT_60",
            ),
            interactions=(
                (
                    "Enter 'help' for a list of built-in commands.",
                    ShellCommand(b"echo LITEOS_BUSYBOX_SHELL_$((6*7))\n"),
                ),
                (
                    "LITEOS_BUSYBOX_SHELL_42",
                    ShellCommand((
                        "value='标准，循环直至达成。任务转译示例：加校验修 bug 重构 X "
                        "多步任务必须先给计划强成功标准让你独立闭环弱标准会反复返工'; "
                        "[ \"$value\" = '标准，循环直至达成。任务转译示例：加校验修 bug 重构 X "
                        "多步任务必须先给计划强成功标准让你独立闭环弱标准会反复返工' ] && "
                        "echo LITEOS_BUSYBOX_UNICODE_$((6*7))\n"
                    ).encode()),
                ),
                (
                    "LITEOS_BUSYBOX_UNICODE_42",
                    ShellCommand(b"/bin/sh /var/tmp/dhcp-gate.sh\n"),
                ),
                (
                    "LITEOS_DHCP_51",
                    ShellCommand(b"count=$(/bin/ps | /bin/grep '[u]dhcpc' | /bin/wc -l); [ \"$count\" -eq 1 ] && echo LITEOS_DHCP_SINGLE_$((7*7+2))\n"),
                ),
                (
                    "LITEOS_DHCP_SINGLE_51",
                    # PID publication precedes DHCP lease/route commit. Poll the complete final
                    # state for the same bounded 20 seconds as dhcp-gate.sh; a fixed sleep makes
                    # normal host-network scheduling jitter look like a kernel regression.
                    ShellCommand(b"old=$(/bin/cat /run/udhcpc.eth0.pid); /bin/kill \"$old\"; attempt=0; while :; do new=$(/bin/cat /run/udhcpc.eth0.pid 2>/dev/null); count=$(/bin/ps | /bin/grep '[u]dhcpc' | /bin/wc -l); if [ -n \"$new\" ] && [ \"$new\" != \"$old\" ] && [ \"$count\" -eq 1 ] && [ -s /etc/resolv.conf ] && /bin/route -n | /bin/grep -q '^0.0.0.0 .*10.0.2.2'; then break; fi; attempt=$((attempt+1)); [ \"$attempt\" -lt 20 ] || break; /bin/sleep 1; done; [ \"$new\" != \"$old\" ] && [ \"$count\" -eq 1 ] && [ -s /etc/resolv.conf ] && /bin/route -n | /bin/grep -q '^0.0.0.0 .*10.0.2.2' && echo LITEOS_DHCP_RESPAWN_$((7*7+2))\n"),
                ),
                (
                    "LITEOS_DHCP_RESPAWN_51",
                    ShellCommand(b"/bin/ping -c 3 -W 3 10.0.2.2 | /bin/grep -q '3 packets received' && echo LITEOS_PING_$((7*7+3))\n"),
                ),
                (
                    "LITEOS_PING_52",
                    ShellCommand(b"/bin/dynamic-smoke resolve example.com\n"),
                ),
                (
                    "LITEOS_DNS_51",
                    ShellCommand(f"/bin/wget -q -T 10 -O /http.out http://10.0.2.2:{http_port}/user/base/udhcpc.script && /bin/grep -q '^# @description BusyBox udhcpc' /http.out && echo LITEOS_HTTP_$((7*7+2))\n".encode()),
                ),
                (
                    "LITEOS_HTTP_51",
                    ShellCommand(f"if /bin/wget -q -T 10 -O /tls-reject.out https://10.0.2.2:{https_port}/user/base/udhcpc.script; then false; else echo LITEOS_TLS_REJECT_$((7*7+3)); fi\n".encode()),
                ),
                (
                    "LITEOS_TLS_REJECT_52",
                    ShellCommand(f"/bin/wget -q -T 10 -O /https.out https://liteos-gate.test:{https_port}/user/base/udhcpc.script && /bin/grep -q '^# @description BusyBox udhcpc' /https.out && echo LITEOS_HTTPS_$((7*7+3))\n".encode()),
                ),
                (
                    "LITEOS_HTTPS_52",
                    ShellCommand(b"mkdir -p /phase53/src /phase53/out /phase53/zip; name=long-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; printf payload53 > /phase53/src/$name; chmod 640 /phase53/src/$name; ln /phase53/src/$name /phase53/src/hard; ln -s $name /phase53/src/soft; tar -czf /phase53/bundle.tar.gz -C /phase53/src . && tar -xzf /phase53/bundle.tar.gz -C /phase53/out; first=$(ls -i /phase53/out/hard | awk '{print $1}'); second=$(ls -i /phase53/out/$name | awk '{print $1}'); mode=$(ls -l /phase53/out/$name | awk '{print $1}'); cmp /phase53/src/$name /phase53/out/$name && [ \"$first\" = \"$second\" ] && [ \"$mode\" = -rw-r----- ] && [ \"$(readlink /phase53/out/soft)\" = \"$name\" ] && [ \"$(realpath /phase53/out/soft)\" = /phase53/out/$name ] && echo LITEOS_TAR_$((7*7+4))\n"),
                ),
                (
                    "LITEOS_TAR_53",
                    ShellCommand(b"printf bzip-phase-53 > /phase53/bzip.txt; bzip2 -c /phase53/bzip.txt > /phase53/bzip.txt.bz2; bzcat /phase53/bzip.txt.bz2 | cmp - /phase53/bzip.txt && xzcat /var/tmp/phase53.xz | grep -q '^xz-phase-53$' && unzip -q /var/tmp/phase53.zip -d /phase53/zip && grep -q '^zip-phase-53$' /phase53/zip/nested/zip.txt && echo LITEOS_COMPRESSION_$((7*7+4))\n"),
                ),
                (
                    "LITEOS_COMPRESSION_53",
                    ShellCommand(b"env PHASE=53 sh -c 'printf %s \"$PHASE\"' | grep -q '^53$' && printf 'one\\ntwo\\n' | xargs printf '%s-' | grep -q 'one-two-' && [ \"$(which tar)\" = /bin/tar ] && du -k /phase53/out >/phase53/du.out && echo LITEOS_TOOLS_$((7*7+4))\n"),
                ),
                (
                    "LITEOS_TOOLS_53",
                    ShellCommand(b"tar -xf /var/tmp/phase53-traversal.tar -C /phase53/out 2>/dev/null || true; [ ! -e /phase53-escape ] && [ ! -e /phase53/phase53-escape ] && echo LITEOS_ARCHIVE_$((7*7+4))\n"),
                ),
                (
                    "LITEOS_ARCHIVE_53",
                    ShellCommand(b"printf 'alpha\\nbeta\\n' >/vi54; vi /vi54; [ \"$(tail -n1 /vi54)\" = OK ] && stty -a | grep -q echo && echo LITEOS_VI_$((9*6))\n"),
                ),
                (
                    "- /vi54 1/2 50%",
                    TerminalInput(b"GoOK"),
                ),
                (
                    "I /vi54 [Modified] 3/3 100%",
                    TerminalInput(b"\x1b"),
                ),
                (
                    "- /vi54 [Modified] 3/3 100%",
                    TerminalInput(b":wq\n"),
                ),
                (
                    "LITEOS_VI_54",
                    ShellCommand(b"seq 1 40 >/less54; less /less54; stty -a | grep -q echo && echo LITEOS_LESS_$((9*6))\n"),
                ),
                (
                    "\x1b[24;0H\x1b[K/less54",
                    TerminalInput(b"q"),
                ),
                (
                    "LITEOS_LESS_54",
                    ShellCommand(b"printf 'old\\n' >/patch54; printf 'new\\n' >/expected54; diff -u /patch54 /expected54 >/update54.patch; status=$?; [ \"$status\" -eq 1 ] && patch /patch54 </update54.patch && cmp /patch54 /expected54 && hexdump -C /patch54 | grep -q '6e 65 77' && od -An -tx1 /patch54 | grep -q '6e 65 77' && strings /bin/busybox | grep -q BusyBox && more /patch54 | grep -q new && clear; reset; stty -a | grep -q echo && echo LITEOS_TEXT_DIAG_$((9*6))\n"),
                ),
                (
                    "LITEOS_TEXT_DIAG_54",
                    ShellCommand(b"/bin/ls /; echo LITEOS_LS_$((6*7))\n"),
                ),
                (
                    "LITEOS_LS_42",
                    ShellCommand(b"/bin/ls /dev; echo HIDDEN >/dev/null; echo LITEOS_NULL_$((6*7)); /bin/dd if=/dev/zero of=/zero bs=4 count=1 2>/dev/null; set -- $(/bin/wc -c /zero); echo LITEOS_ZERO_$1; echo LITEOS_TTYDEV_$((6*7)) >/dev/tty; echo LITEOS_CONSOLEDEV_$((6*7)) >/dev/console; cd /dev; set -- $(/bin/pwd); [ \"$1\" = /dev ] && echo LITEOS_DEVCWD_$((6*7)); cd /\n"),
                ),
                (
                    "LITEOS_DEVCWD_42",
                    ShellCommand(b"/bin/printf 'pear\\napple\\napple\\n' > /words; set -- $(/bin/sort /words | /bin/uniq | /bin/wc -l); [ \"$1\" = 2 ] && echo LITEOS_TEXT_$((6*7))\n"),
                ),
                (
                    "LITEOS_TEXT_42",
                    ShellCommand(b"/bin/printf 'a:1\\nb:2\\nc:3\\n' | /bin/tee /data >/dev/null; a=$(/bin/sed -n '2p' /data | /bin/cut -d: -f2 | /bin/tr 2 7); b=$(/bin/awk -F: '{s+=$2} END {print s}' /data); c=$(/bin/head -n1 /data); d=$(/bin/tail -n1 /data); [ \"$a:$b:$c:$d\" = '7:6:a:1:c:3' ] && echo LITEOS_FILTERS_$((6*7))\n"),
                ),
                (
                    "LITEOS_FILTERS_42",
                    ShellCommand(b"/bin/mkdir /tools; /bin/touch /tools/a; n=$(/bin/find /tools -name a | /bin/wc -l); base=$(/bin/basename /a/b); dir=$(/bin/dirname /a/b); [ \"$n:$base:$dir\" = '1:b:/a' ] && echo LITEOS_FIND_$((6*7))\n"),
                ),
                (
                    "LITEOS_FIND_42",
                    ShellCommand(b"e=$(/bin/expr 6 \\* 7); last=$(/bin/seq 41 42 | /bin/tail -n1); [ \"$e:$last\" = '42:42' ] && echo LITEOS_MATH_$((6*7))\n"),
                ),
                (
                    "LITEOS_MATH_42",
                    ShellCommand(b"/bin/sleep 0; echo LITEOS_TOOLS_$((6*7))\n"),
                ),
                (
                    "LITEOS_TOOLS_42",
                    ShellCommand(b"/bin/ps > /ps.out; proc_total=$(/bin/awk '/^MemTotal:/ {print $2}' /proc/meminfo); free_total=$(/bin/free | /bin/awk '/^Mem:/ {print $2}'); /bin/grep -q init /ps.out && /bin/grep -q sh /ps.out && [ \"$proc_total\" = \"$free_total\" ] && /bin/uptime | /bin/grep -q 'load average:' && echo LITEOS_OBSERVABILITY_$((6*7))\n"),
                ),
                # 完整记录应包含实际 relatime 和 ext4 data=ordered；proc 可写策略由节点权限约束。
                (
                    "LITEOS_OBSERVABILITY_42",
                    ShellCommand(b"/bin/grep -q '^/dev/vda / ext4 rw,relatime,data=ordered 0 0$' /proc/mounts && /bin/grep -q '^devtmpfs /dev devtmpfs ro,relatime 0 0$' /proc/mounts && /bin/grep -q '^proc /proc proc rw,relatime 0 0$' /proc/mounts && /bin/grep -q '^sysfs /sys sysfs ro,relatime 0 0$' /proc/mounts && /bin/grep -q '^devpts /dev/pts devpts rw,relatime 0 0$' /proc/mounts && /bin/df -Pk > /df.before; set -- $(/bin/awk 'NR==2 {print $2, $4, $6}' /df.before); total=$1; before=$2; mounted=$3; /bin/dd if=/dev/zero of=/df-space bs=4096 count=2 2>/dev/null; after=$(/bin/df -Pk / | /bin/awk 'NR==2 {print $4}'); inodes=$(/bin/df -Pi / | /bin/awk 'NR==2 {print $2}'); /bin/rm -f /df-space; [ ! -e /df-space ] && [ \"$total\" -gt 0 ] && [ \"$before\" -gt \"$after\" ] && [ \"$inodes\" -gt 0 ] && [ \"$mounted\" = / ] && echo LITEOS_FILESYSTEM_CAPACITY_$((6*7))\n"),
                ),
                (
                    "LITEOS_FILESYSTEM_CAPACITY_42",
                    ShellCommand(b'/bin/mkdir -p /mnt/proc2 && /bin/mount -t proc proc /mnt/proc2 && [ -r /mnt/proc2/meminfo ] && [ "$(/bin/stat -c %d /mnt/proc2)" != "$(/bin/stat -c %d /proc)" ] && cd /mnt/proc2 && ! /bin/umount /mnt/proc2 2>/dev/null && cd / && /bin/umount /mnt/proc2 && [ ! -e /mnt/proc2/meminfo ] && ! /bin/mount -t nosuchfs none /mnt/proc2 2>/dev/null && /bin/mount -t ext4 /dev/vda /mnt/proc2 && [ "$(/bin/stat -c \'%d:%i\' /mnt/proc2)" = "$(/bin/stat -c \'%d:%i\' /)" ] && echo shared-owner >/mnt/proc2/mount-owner && [ "$(/bin/cat /mount-owner)" = shared-owner ] && /bin/umount /mnt/proc2 && /bin/rm /mount-owner && { exec 9>/root-umount-writer; ! /bin/umount / 2>/dev/null; busy=$?; exec 9>&-; /bin/rm /root-umount-writer; [ "$busy" -eq 0 ]; } && /bin/ls -l /dev/vda | /bin/grep -q \'^b\' && echo LITEOS_MOUNT_$((6*7+2))\n'),
                ),
                (
                    "LITEOS_MOUNT_44",
                    ShellCommand((
                        f"[ \"$(/bin/cat /proc/cmdline)\" = '{BUSYBOX_KERNEL_ARGUMENTS}' ] && "
                        "[ \"$LITEOS_BOOT_ENV\" = 42 ] && echo LITEOS_CMDLINE_$((6*7+3))\n"
                    ).encode()),
                ),
                (
                    "LITEOS_CMDLINE_45",
                    ShellCommand(b"/bin/mkdir -p /mnt/t && /bin/mount -t tmpfs -o size=1m,nr_inodes=24,mode=0700 tmpfs /mnt/t && [ \"$(/bin/stat -f -c %T /mnt/t)\" = tmpfs ] && [ \"$(/bin/stat -c %a /mnt/t)\" = 700 ] && [ \"$(/bin/stat -c %d /mnt/t)\" != \"$(/bin/stat -c %d /)\" ] && [ \"$(/bin/stat -f -c %T /run)\" = tmpfs ] && [ \"$(/bin/stat -f -c %T /tmp)\" = tmpfs ] && [ \"$(/bin/stat -f -c %T /dev/shm)\" = tmpfs ] && [ \"$(/bin/stat -c %a /tmp)\" = 1777 ] && ! /bin/mount -t tmpfs -o huge=always tmpfs /mnt/t 2>/dev/null && echo LITEOS_TMPFS_$((6*7+4))\n"),
                ),
                (
                    "LITEOS_TMPFS_46",
                    ShellCommand(b"echo hello >/mnt/t/a && /bin/ln /mnt/t/a /mnt/t/b && /bin/mkdir /mnt/t/d && /bin/mv /mnt/t/b /mnt/t/d/c && [ \"$(/bin/cat /mnt/t/d/c)\" = hello ] && [ \"$(/bin/stat -c %h /mnt/t/a)\" = 2 ] && /bin/ln -s a /mnt/t/s && [ \"$(/bin/readlink /mnt/t/s)\" = a ] && ! /bin/mv /mnt/t/d /mnt/t/d/sub 2>/dev/null && [ \"$(/bin/ls /mnt/t | /bin/wc -l)\" = 3 ] && echo LITEOS_TMPFS_$((6*7+5))\n"),
                ),
                (
                    "LITEOS_TMPFS_47",
                    ShellCommand(b"! /bin/dd if=/dev/zero of=/mnt/t/big bs=64k count=32 2>/dev/null && /bin/rm /mnt/t/big && /bin/dd if=/dev/zero of=/mnt/t/ok bs=64k count=8 2>/dev/null && [ \"$(/bin/stat -c %s /mnt/t/ok)\" = 524288 ] && /bin/rm /mnt/t/ok && exec 3>/mnt/t/held && /bin/dd if=/dev/zero bs=64k count=12 >&3 2>/dev/null && /bin/rm /mnt/t/held && ! /bin/dd if=/dev/zero of=/mnt/t/more bs=64k count=8 2>/dev/null && exec 3>&- && /bin/rm -f /mnt/t/more && /bin/dd if=/dev/zero of=/mnt/t/after bs=64k count=8 2>/dev/null && /bin/rm /mnt/t/after && echo LITEOS_TMPFS_$((6*7+6))\n"),
                ),
                (
                    "LITEOS_TMPFS_48",
                    ShellCommand(b"n=0; while /bin/touch /mnt/t/f$n 2>/dev/null; do n=$((n+1)); done; [ $n -gt 0 ] && [ $n -lt 24 ] && /bin/rm /mnt/t/f0 && /bin/touch /mnt/t/again && cd /mnt/t && ! /bin/umount /mnt/t 2>/dev/null && cd / && /bin/umount /mnt/t && [ ! -e /mnt/t/a ] && echo LITEOS_TMPFS_$((6*7+7))\n"),
                ),
                (
                    "LITEOS_TMPFS_49",
                    ShellCommand(b"/bin/mkdir -p /mnt/t2 && /bin/mount -t tmpfs tmpfs /mnt/t2 && cd /mnt/t2 && i=0; while [ $i -lt 600 ] && /bin/mkdir d 2>/dev/null && cd d; do i=$((i+1)); done; cd /; /bin/umount /mnt/t2 && [ $i -eq 600 ] && echo LITEOS_TMPFS_DEEP_$((6*7+10))\n"),
                ),
                (
                    "LITEOS_TMPFS_DEEP_52",
                    ShellCommand(b"free() { /bin/df -kP / | /bin/tail -n 1 | { read _ _ _ a _; echo $a; }; }; b=$(free); /bin/dd if=/dev/zero of=/rel bs=64k count=64 2>/dev/null; /bin/cat /rel >/dev/null; m=$(free); /bin/rm /rel; a=$(free); [ $((b-m)) -ge 3000 ] && [ $((a-b)) -le 256 ] && [ $((b-a)) -le 256 ] && echo LITEOS_UNLINK_RELEASE_$((6*7+8)) || echo LITEOS_UNLINK_RELEASE_DIAG first b=$b m=$m a=$a\n"),
                ),
                (
                    "LITEOS_UNLINK_RELEASE_50",
                    ShellCommand(b"/bin/dd if=/dev/zero of=/rel2 bs=64k count=64 2>/dev/null; /bin/cat /rel2 >/dev/null; exec 5</rel2; c=$(free); /bin/rm /rel2; d=$(free); [ $((d-c)) -le 256 ] && exec 5<&- && e=$(free) && [ $((e-d)) -ge 3000 ] && echo LITEOS_UNLINK_RELEASE_$((6*7+9)) || echo LITEOS_UNLINK_RELEASE_DIAG second c=$c d=$d e=${e:-unset}\n"),
                ),
                (
                    "LITEOS_UNLINK_RELEASE_51",
                    ShellCommand(b'echo retained >/rel3 && /bin/dd if=/dev/zero bs=64k count=64 >>/rel3 2>/dev/null && /bin/cat /rel3 >/dev/null && exec 6<>/rel3 && c=$(free) && /bin/rm /rel3 && d=$(free) && [ $((d-c)) -le 256 ] && [ "$(/bin/dd bs=8 count=1 <&6 2>/dev/null)" = retained ] && exec 6>&- && e=$(free) && [ $((e-d)) -ge 3000 ] && /bin/dd if=/dev/zero of=/rename-victim bs=64k count=64 2>/dev/null && /bin/cat /rename-victim >/dev/null && echo replacement >/rename-source && c=$(free) && /bin/mv /rename-source /rename-victim && e=$(free) && [ $((e-c)) -ge 3000 ] && [ "$(/bin/cat /rename-victim)" = replacement ] && /bin/rm /rename-victim && echo LITEOS_CACHE_RETIREMENT_$((6*10+1))\n'),
                ),
                (
                    "LITEOS_CACHE_RETIREMENT_61",
                    ShellCommand(b"/bin/mkdir -p /mnt/r && /bin/mount -t tmpfs -o size=1m tmpfs /mnt/r && echo data >/mnt/r/f && printf '#!/bin/sh\\necho ran\\n' >/mnt/r/x && /bin/chmod 755 /mnt/r/x && [ \"$(/mnt/r/x)\" = ran ] && /bin/mount -t tmpfs -o remount,noexec tmpfs /mnt/r && ! /mnt/r/x 2>/dev/null && /bin/grep -q ' /mnt/r tmpfs rw,noexec,relatime,size=1024k ' /proc/mounts && /bin/mount -t tmpfs -o remount,ro tmpfs /mnt/r && /bin/grep -q ' /mnt/r tmpfs ro,relatime,size=1024k ' /proc/mounts && ! echo y >/mnt/r/g 2>/dev/null && ! /bin/rm /mnt/r/f 2>/dev/null && ! /bin/mv /mnt/r/f /mnt/r/h 2>/dev/null && ! /bin/chmod 600 /mnt/r/f 2>/dev/null && ! /bin/touch /mnt/r/f 2>/dev/null && [ \"$(/bin/cat /mnt/r/f)\" = data ] && echo LITEOS_MOUNTFLAGS_$((6*7+11))\n"),
                ),
                (
                    "LITEOS_MOUNTFLAGS_53",
                    ShellCommand(b"/bin/mount -t tmpfs -o remount,rw tmpfs /mnt/r && echo ok >/mnt/r/g && exec 3>/mnt/r/h && ! /bin/mount -t tmpfs -o remount,ro tmpfs /mnt/r 2>/dev/null && exec 3>&- && /bin/mount -t tmpfs -o remount,ro tmpfs /mnt/r && [ \"$(/bin/cat /mnt/r/g)\" = ok ] && echo LITEOS_MOUNTFLAGS_$((6*7+12))\n"),
                ),
                (
                    "LITEOS_MOUNTFLAGS_54",
                    ShellCommand(b"/bin/mount -t tmpfs -o remount,rw,size=2m tmpfs /mnt/r && /bin/dd if=/dev/zero of=/mnt/r/big bs=64k count=24 2>/dev/null && ! /bin/mount -t tmpfs -o remount,size=64k tmpfs /mnt/r 2>/dev/null && [ \"$(/bin/stat -f -c %b /mnt/r)\" = 512 ] && /bin/rm /mnt/r/big && /bin/mount -t tmpfs -o remount,size=64k tmpfs /mnt/r && [ \"$(/bin/stat -f -c %b /mnt/r)\" = 16 ] && /bin/umount /mnt/r && echo LITEOS_MOUNTFLAGS_$((6*7+13))\n"),
                ),
                (
                    "LITEOS_MOUNTFLAGS_55",
                    ShellCommand(b"exec 3</dev/kmsg && echo '<5>liteos-kmsg-probe' >/dev/kmsg && i=0 && while [ $i -lt 400 ]; do /bin/dd bs=4096 count=1 <&3 >/var/tmp/k 2>/dev/null; /bin/grep -q liteos-kmsg-probe /var/tmp/k && break; i=$((i+1)); done; /bin/grep -q '^13,[0-9]*,[0-9]*,-;liteos-kmsg-probe$' /var/tmp/k && echo LITEOS_KMSG_$((6*7+14))\n"),
                ),
                (
                    "LITEOS_KMSG_56",
                    ShellCommand(b"(/bin/sleep 1; echo '<6>liteos-kmsg-wake' >/dev/kmsg) & i=0; while [ $i -lt 400 ]; do /bin/dd bs=4096 count=1 <&3 >/var/tmp/k3 2>/dev/null; /bin/grep -q 'liteos-kmsg-wake$' /var/tmp/k3 && break; i=$((i+1)); done; wait; exec 3<&-; /bin/grep -q '^14,[0-9]*,[0-9]*,-;liteos-kmsg-wake$' /var/tmp/k3 && echo LITEOS_KMSG_$((6*7+15))\n"),
                ),
                (
                    "LITEOS_KMSG_57",
                    ShellCommand(b"[ \"$(/bin/dd if=/dev/vdb bs=1 count=26 2>/dev/null)\" = LITEOS-SCRATCH-DISK-HEADER ] && [ \"$(/bin/stat -c %t:%T /dev/vdb)\" = fe:10 ] && printf ABCDEFGHIJ | /bin/dd of=/dev/vdb bs=1 seek=4093 2>/dev/null && [ \"$(/bin/dd if=/dev/vdb bs=1 skip=4093 count=10 2>/dev/null)\" = ABCDEFGHIJ ] && [ \"$(/bin/dd if=/dev/vdb bs=1 count=26 2>/dev/null)\" = LITEOS-SCRATCH-DISK-HEADER ] && echo LITEOS_BLOCK_$((6*7+16))\n"),
                ),
                (
                    "LITEOS_BLOCK_58",
                    ShellCommand(b"echo marker-at-tail | /bin/dd of=/dev/vdb bs=1 seek=16777202 2>/dev/null; ! /bin/dd if=/dev/zero of=/dev/vdb bs=4096 seek=4096 count=1 2>/dev/null && /bin/sync && [ \"$(/bin/dd if=/dev/vdb bs=1 skip=16777202 count=11 2>/dev/null)\" = marker-at-t ] && echo LITEOS_BLOCK_$((6*7+17))\n"),
                ),
                (
                    "LITEOS_BLOCK_59",
                    ShellCommand(b"/bin/dd if=/dev/zero of=/dev/vdb bs=64k count=64 2>/dev/null && [ \"$(/bin/dd if=/dev/vdb bs=64k count=64 2>/dev/null | /bin/tr -d '\\000' | /bin/wc -c)\" -eq 0 ] && /bin/mkdir -p /mnt/b && exec 4>>/dev/vdb && ! /bin/mount -t ext4 /dev/vdb /mnt/b 2>/dev/null && exec 4>&- && ! /bin/dd if=/dev/zero of=/dev/vda bs=512 count=1 2>/dev/null && [ \"$(/bin/dd if=/dev/vda bs=1 skip=1080 count=2 2>/dev/null | /bin/tr '\\123\\357' 'SE')\" = SE ] && echo LITEOS_BLOCK_$((6*7+18))\n"),
                ),
                (
                    "LITEOS_BLOCK_60",
                    ShellCommand(b"[ \"$(/bin/stat -c %t:%T /dev/vdb1)\" = fe:11 ] && [ \"$(/bin/stat -c %t:%T /dev/vdb2)\" = fe:12 ] && /bin/mkdir -p /mnt/p && /bin/mount -t ext4 /dev/vdb2 /mnt/p && echo on-partition >/mnt/p/f && [ \"$(/bin/cat /mnt/p/f)\" = on-partition ] && ! /bin/dd if=/dev/zero of=/dev/vdb2 bs=512 count=1 2>/dev/null && /bin/umount /mnt/p && /bin/mount -t ext4 /dev/vdb2 /mnt/p && [ \"$(/bin/cat /mnt/p/f)\" = on-partition ] && /bin/umount /mnt/p && echo LITEOS_PARTITION_$((6*7+19))\n"),
                ),
                (
                    "LITEOS_PARTITION_61",
                    ShellCommand(b"/bin/grep -q ' 10240 vdb2$' /proc/partitions && /bin/grep -q ' 4096 vdb1$' /proc/partitions && [ \"$(/bin/grep -c ' vd' /proc/partitions)\" = 4 ] && [ \"$(/bin/cat /sys/class/block/vdb2/dev)\" = 254:18 ] && [ \"$(/bin/cat /sys/class/block/vdb2/size)\" = 20480 ] && [ \"$(/bin/cat /sys/class/block/vdb2/start)\" = 10240 ] && [ \"$(/bin/cat /sys/class/block/vdb2/partition)\" = 2 ] && [ \"$(/bin/cat /sys/block/vdb/vdb2/start)\" = 10240 ] && [ -e /sys/block/vdb/size ] && [ ! -e /sys/block/vdb2 ] && [ \"$(/bin/grep DEVTYPE /sys/dev/block/254:18/uevent)\" = DEVTYPE=partition ] && /bin/grep -q '^PARTUUID=[0-9a-f]\\{8\\}-02$' /sys/block/vdb/vdb2/uevent && [ \"$(/bin/ls /sys/block | /bin/wc -l)\" = 2 ] && [ \"$(/bin/ls /sys/class/block | /bin/wc -l)\" = 4 ] && echo LITEOS_BLOCKSYS_$((6*7+20))\n"),
                ),
                (
                    "LITEOS_BLOCKSYS_62",
                    ShellCommand(b"/var/tmp/memory-file-probe\n"),
                ),
                (
                    "LITEOS_MEMFILE_42",
                    ShellCommand(b"/var/tmp/special-file-probe\n"),
                ),
                (
                    "LITEOS_SPECIAL_42",
                    ShellCommand(b"/var/tmp/inotify-probe\n"),
                ),
                (
                    "LITEOS_INOTIFY_42",
                    ShellCommand(b"/var/tmp/fuse-probe\n"),
                ),
                (
                    "LITEOS_FUSE_42",
                    ShellCommand(b"/bin/rm -rf /links; /bin/mkdir /links; echo alpha >/links/source; /bin/ln /links/source /links/hard; /bin/ln -s source /links/soft; [ \"$(/bin/cat /links/hard)\" = alpha ] && [ \"$(/bin/cat /links/soft)\" = alpha ] && echo beta >/links/hard; /bin/rm /links/source; [ \"$(/bin/cat /links/hard)\" = beta ] && /bin/ls -l /links/soft | /bin/grep -q -- '-> source' && echo LITEOS_LINKS_$((6*7+1))\n"),
                ),
                (
                    "LITEOS_LINKS_43",
                    ShellCommand(b"/bin/rm -rf /race; /bin/mkdir /race; workers=''; i=0; while [ $i -lt 8 ]; do (/bin/mkdir /race/d$i; j=0; while [ $j -lt 4 ]; do echo payload >/race/d$i/source$j && /bin/cp /race/d$i/source$j /race/d$i/copy$j && /bin/ln /race/d$i/copy$j /race/d$i/hard$j && /bin/ln -s hard$j /race/d$i/sym$j && /bin/mv /race/d$i/copy$j /race/d$i/moved$j && [ \"$(/bin/cat /race/d$i/sym$j)\" = payload ] && /bin/rm /race/d$i/sym$j /race/d$i/hard$j /race/d$i/moved$j /race/d$i/source$j || exit 1; j=$((j+1)); done; /bin/rmdir /race/d$i; echo done >>/race/completed) & workers=\"$workers $!\"; i=$((i+1)); done; ok=1; for worker in $workers; do wait $worker || ok=0; done; count=$(/bin/wc -l </race/completed); [ \"$ok:$count\" = 1:8 ] && echo LITEOS_NAMESPACE_CONCURRENCY_$((6*7+1))\n"),
                ),
                (
                    "LITEOS_NAMESPACE_CONCURRENCY_43",
                    ShellCommand(b"[ \"$(/bin/id -u):$(/bin/id -g)\" = 0:0 ] && /bin/touch /busybox-credential && /bin/chmod 640 /busybox-credential && /bin/chown 1000:1000 /busybox-credential && set -- $(/bin/ls -ln /busybox-credential); [ \"$1:$3:$4\" = '-rw-r-----:1000:1000' ] && echo LITEOS_BUSYBOX_CREDENTIALS_$((6*7+2))\n"),
                ),
                (
                    "LITEOS_BUSYBOX_CREDENTIALS_44",
                    ShellCommand((
                        "[ \"$(/bin/uname -s)\" = LiteOS ] && "
                        "[ \"$(/bin/uname -n)\" = liteos ] && "
                        f"[ \"$(/bin/uname -m)\" = {TARGET.arch} ] && "
                        "[ \"$(/bin/uname -o)\" = LiteOS ] && "
                        f"[ \"$(/bin/arch)\" = {TARGET.arch} ] && "
                        "echo LITEOS_SYSTEM_IDENTITY_$((6*7))\n"
                    ).encode()),
                ),
                (
                    "LITEOS_SYSTEM_IDENTITY_42",
                    ShellCommand(b"epoch=$(/bin/date -u +%s); year=$(/bin/date -u +%Y); [ \"$epoch\" -ge 1704067200 ] && [ \"$year\" -ge 2024 ] && echo LITEOS_WALLCLOCK_$((6*7))\n"),
                ),
                (
                    "LITEOS_WALLCLOCK_42",
                    ShellCommand(b"{ read _; read _ free_before _; } </proc/meminfo; i=0; while [ $i -lt 64 ] && /bin/true; do i=$((i+1)); done; { read _; read _ free_after _; } </proc/meminfo; loss=$((free_before-free_after)); [ \"$i\" -eq 64 ] && [ \"$loss\" -le 4096 ] && echo LITEOS_EXEC_RECLAIM_$((6*7))\n"),
                ),
                (
                    "LITEOS_EXEC_RECLAIM_42",
                    ShellCommand(b"/bin/top -bn1 | /bin/grep -q init && echo LITEOS_TOP_$((6*7))\n"),
                ),
                (
                    "LITEOS_TOP_42",
                    ShellCommand((
                        f"/bin/ls -l /lib | /bin/grep -q "
                        f"'{Path(TARGET.musl_loader).name} -> /usr/lib/libc.so' && "
                        "echo LITEOS_READLINK_$((6*7))\n"
                    ).encode()),
                ),
                (
                    "LITEOS_READLINK_42",
                    ShellCommand(b"/bin/dynamic-smoke\n"),
                ),
                (
                    "LITEOS_UNIX_EPOLL_46",
                    TerminalInput(b""),
                ),
                (
                    "LITEOS_DLOPEN_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/dynamic-smoke spawn\n"),
                ),
                (
                    "LITEOS_POSIX_SPAWN_59",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/liteos-script 'alpha beta' omega\n"),
                ),
                (
                    "LITEOS_SCRIPT_EXEC_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/rm -f /kill.pid; /bin/setsid /bin/sh -c 'echo $$ >/kill.pid; trap \"echo LITEOS_KILL_GROUP_$((6*7)); exit 0\" TERM; echo LITEOS_KILL_\"READY\"; while :; do /bin/sleep 1; done' &\n"),
                ),
                (
                    "LITEOS_KILL_READY",
                    ShellCommand(b"read kill_pid < /kill.pid; /bin/kill -0 $kill_pid && /bin/kill -TERM -$kill_pid\n"),
                ),
                (
                    "LITEOS_KILL_GROUP_42",
                    ShellCommand(b"/bin/echo payload > /plain; /bin/gzip -c /plain > /plain.gz; a=$(/bin/zcat /plain.gz); b=$(/bin/gunzip -c /plain.gz); h=$(/bin/sha256sum /plain | /bin/cut -d' ' -f1); [ \"$a:$b:$h\" = 'payload:payload:d4e4877bac978b7952f0d544fc52ebff5411d351d129f1f056fa43f11da9af2b' ] && echo LITEOS_ARCHIVE_$((6*7))\n"),
                ),
                (
                    "LITEOS_ARCHIVE_42",
                    ShellCommand(b"/bin/echo LITEOS_PIPE_$((6*7)) | /bin/grep PIPE; echo LITEOS_REDIR_$((6*7)) > /redir; /bin/cat /redir; (echo LITEOS_BG_$((6*7)) > /bg) & wait; /bin/cat /bg; echo LITEOS_PERSIST_$((6*7)) > /persist; sync; echo LITEOS_PERSIST_WRITTEN_$((6*7))\n"),
                ),
                (
                    "LITEOS_PERSIST_WRITTEN_42",
                    ShellCommand(b"/bin/dynamic-smoke job-control\n"),
                ),
                (
                    "LITEOS_JOB_START_42",
                    TerminalInput(b"\x1a"),
                ),
                (
                    "Stopped",
                    ShellCommand(b"jobs > /jobs; /bin/grep -q Stopped /jobs && echo LITEOS_JOBS_STOPPED_$((6*7))\n"),
                ),
                (
                    "LITEOS_JOBS_STOPPED_42",
                    ShellCommand(b"bg; jobs > /jobs; /bin/grep -q Running /jobs && echo LITEOS_BG_CONTINUED_$((6*7))\n"),
                ),
                (
                    "LITEOS_BG_CONTINUED_42",
                    ShellCommand(b"fg\n"),
                ),
                (
                    "LITEOS_FG_START_42",
                    TerminalInput(b"\x03"),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"echo LITEOS_FG_CTRL_C_$((6*7)); echo LITEOS_TTY_CTRL_C_$((6*7))\n"),
                ),
                (
                    "LITEOS_TTY_CTRL_C_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/dd if=/dev/tty of=/bgread bs=16 count=1 2>/dev/null & echo LITEOS_BG_READ_LAUNCHED_$((6*7))\n"),
                ),
                (
                    "LITEOS_BG_READ_LAUNCHED_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/sleep 1; jobs > /jobs; /bin/grep -q Stopped /jobs && echo LITEOS_BG_READ_STOPPED_$((6*7))\n"),
                ),
                (
                    "LITEOS_BG_READ_STOPPED_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"fg\n"),
                ),
                (
                    "/bin/dd if=/dev/tty",
                    TerminalInput(b"ttyinput\n"),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/grep -q '^ttyinput$' /bgread && echo LITEOS_BG_READ_OK_$((6*7))\n"),
                ),
                (
                    "LITEOS_BG_READ_OK_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/sh -c 'trap \"\" 21; exec /bin/dd if=/dev/tty of=/ignored-read bs=1 count=1 2>/dev/null' & wait; [ ! -s /ignored-read ] && echo LITEOS_BG_READ_IGNORED_EIO_$((6*7))\n"),
                ),
                (
                    "LITEOS_BG_READ_IGNORED_EIO_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/stty tostop </dev/tty; /bin/echo LITEOS_BG_WRITE_OK_$((6*7)) >/dev/tty & echo LITEOS_BG_WRITE_LAUNCHED_$((6*7))\n"),
                ),
                (
                    "LITEOS_BG_WRITE_LAUNCHED_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/sleep 1; jobs > /jobs; /bin/grep -q Stopped /jobs && echo LITEOS_BG_WRITE_STOPPED_$((6*7))\n"),
                ),
                (
                    "LITEOS_BG_WRITE_STOPPED_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"fg\n"),
                ),
                (
                    "LITEOS_BG_WRITE_OK_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/sh -c 'trap \"\" 22; echo LITEOS_BG_WRITE_IGNORED_$((6*7)) >/dev/tty' & wait; /bin/stty -tostop </dev/tty\n"),
                ),
                (
                    "LITEOS_BG_WRITE_IGNORED_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/kill -KILL 1; /bin/sleep 1; /bin/kill -0 1 && echo LITEOS_INIT_SURVIVED_$((6*7))\n"),
                ),
                (
                    "LITEOS_INIT_SURVIVED_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/rm -f /orphan-result; /bin/sh -c 'trap \"echo LITEOS_ORPHAN_HUP_42 >/orphan-result\" 1; /bin/kill -STOP $$; echo LITEOS_ORPHAN_CONT_42 >>/orphan-result' & echo LITEOS_ORPHAN_LAUNCHED_$((6*7))\n"),
                ),
                (
                    "LITEOS_ORPHAN_LAUNCHED_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/sleep 1; jobs > /jobs; /bin/grep -q Stopped /jobs && echo LITEOS_ORPHAN_STOPPED_$((6*7))\n"),
                ),
                (
                    "LITEOS_ORPHAN_STOPPED_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"exit\n"),
                ),
                (
                    "You have stopped jobs.",
                    ShellCommand(b"exit\n"),
                ),
                (
                    "Enter 'help' for a list of built-in commands.",
                    ShellCommand(b"while [ ! -s /orphan-result ]; do /bin/sleep 1; done; /bin/cat /orphan-result\n"),
                ),
                (
                    "LITEOS_ORPHAN_CONT_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b"/bin/rm -f /session-hup; /bin/sh -c 'trap \"echo LITEOS_SESSION_HUP_42 >/session-hup; exit 0\" 1; /bin/kill -KILL $PPID; while :; do /bin/sleep 1; done'\n"),
                ),
                (
                    "Enter 'help' for a list of built-in commands.",
                    ShellCommand(b"while [ ! -s /session-hup ]; do /bin/sleep 1; done; /bin/cat /session-hup\n"),
                ),
                (
                    "LITEOS_SESSION_HUP_42",
                    TerminalInput(b""),
                ),
                (
                    SHELL_PROMPT,
                    ShellCommand(b'echo preserved >/root-umount-canary && /bin/umount / && /bin/grep -q \'^/dev/vda / ext4 ro,relatime,data=ordered 0 0$\' /proc/mounts && /bin/awk \'$5=="/" && $6=="rw,relatime" && $NF=="ro,data=ordered" {ok=1} END {exit !ok}\' /proc/self/mountinfo && ! echo changed >/root-umount-canary 2>/dev/null && [ "$(/bin/cat /root-umount-canary)" = preserved ] && /bin/mount -o remount,rw / && /bin/grep -q \'^/dev/vda / ext4 rw,relatime,data=ordered 0 0$\' /proc/mounts && /bin/rm /root-umount-canary && echo LITEOS_ROOT_REMOUNT_$((6*10))\n'),
                ),
            ),
            forbidden_markers=FORBIDDEN_BOOT_MARKERS,
            persistent_writes=True,
            timeout_seconds=90,
            kernel_arguments=BUSYBOX_KERNEL_ARGUMENTS,
            scratch_disk=scratch_disk,
        )
        boot(
            phase55_image,
            1,
            (
                *cpu_topology_markers(1),
                "init started: BusyBox v1.37.0",
                "LITEOS_PROCESS_TOOLS_55",
            ),
            forbidden_markers=FORBIDDEN_BOOT_MARKERS,
            timeout_seconds=45,
        )
        boot(
            phase56_image,
            1,
            (
                *cpu_topology_markers(1),
                "init started: BusyBox v1.37.0",
                "LITEOS_IDENTITY_56",
            ),
            forbidden_markers=FORBIDDEN_BOOT_MARKERS,
            timeout_seconds=30,
        )
        boot(
            phase57_image,
            1,
            (
                *cpu_topology_markers(1),
                "init started: BusyBox v1.37.0",
                "LITEOS_OPENED_FD_57",
            ),
            forbidden_markers=FORBIDDEN_BOOT_MARKERS,
            timeout_seconds=30,
        )
        boot(
            kernel_stack_churn_image,
            2,
            (
                *cpu_topology_markers(2),
                "init started: BusyBox v1.37.0",
                "LITEOS_KERNEL_STACK_CHURN_42",
            ),
            forbidden_markers=FORBIDDEN_BOOT_MARKERS,
            timeout_seconds=90,
        )
        boot(
            runtime_image,
            8,
            (
                *cpu_topology_markers(8),
                "init started: BusyBox v1.37.0",
                "LITEOS_PERSIST_42",
                "LITEOS_SHARED_PERSIST_45",
                "LITEOS_CREDENTIAL_PERSIST_44",
                "LITEOS_COW_ISOLATION_42",
                "LITEOS_SCHED_8_HARTS_42",
                "LITEOS_STREAMING_EXEC_42",
                "LITEOS_FSSTRESS_42",
            ),
            interactions=(
                (
                    "Enter 'help' for a list of built-in commands.",
                    ShellCommand(b"/bin/cat /persist\n"),
                ),
                (
                    "LITEOS_PERSIST_42",
                    ShellCommand(b"/bin/cat /shared-persist\n"),
                ),
                (
                    "LITEOS_SHARED_PERSIST_45",
                    ShellCommand(b"set -- $(/bin/ls -ln /busybox-credential); [ \"$1:$3:$4\" = '-rw-r-----:1000:1000' ] && echo LITEOS_CREDENTIAL_PERSIST_$((6*7+2))\n"),
                ),
                (
                    "LITEOS_CREDENTIAL_PERSIST_44",
                    ShellCommand(b"x=parent; (x=child; [ \"$x\" = child ]) & wait; [ \"$x\" = parent ] && echo LITEOS_COW_ISOLATION_$((6*7))\n"),
                ),
                (
                    "LITEOS_COW_ISOLATION_42",
                    ShellCommand(b"pids=''; i=0; while [ $i -lt 8 ]; do (while :; do :; done) & pids=\"$pids $!\"; i=$((i+1)); done; echo LITEOS_SCHED_\"STARTED\"\n"),
                ),
                (
                    "LITEOS_SCHED_STARTED",
                    ShellCommand(b"/bin/sleep 2; n=$(/bin/awk '$1 ~ /^cpu[0-9]+$/ && $2 > 0 {n++} END {print n+0}' /proc/stat); echo LITEOS_SCHED_CPUS_$n; [ \"$n\" -eq 8 ] && echo LITEOS_SCHED_8_HARTS_$((6*7))\n"),
                ),
                (
                    "LITEOS_SCHED_8_HARTS_42",
                    ShellCommand(b"n=$(for p in $pids; do /bin/awk '{print $1}' /proc/$p/stat; done | /bin/sort -n | /bin/wc -l); [ \"$n\" -eq 8 ] && echo LITEOS_STREAMING_EXEC_$((6*7))\n"),
                ),
                (
                    "LITEOS_STREAMING_EXEC_42",
                    ShellCommand(b"kill $pids; wait; /var/tmp/fs-stress-probe\n"),
                ),
            ),
            forbidden_markers=FORBIDDEN_BOOT_MARKERS,
            timeout_seconds=90,
            persistent_writes=True,
        )
        apk_crash_image = runtime_path / "apk-crash.img"
        shutil.copyfile(image, apk_crash_image)
        apk_crash_v1, apk_crash_v2, apk_crash_v3 = install_apk_crash_fixtures(
            apk_crash_image,
            find_debugfs(),
            runtime_path,
        )
        power_cut(
            apk_crash_image,
            4,
            (
                f"echo LITEOS_APK_CRASH_ACTIVE; while :; do "
                f"/sbin/apk --no-network add --allow-downgrades /var/tmp/{apk_crash_v1}; "
                f"/sbin/apk --no-network add --upgrade /var/tmp/{apk_crash_v2}; done\n"
            ).encode(),
            "LITEOS_APK_CRASH_ACTIVE",
            0.02,
        )
        boot(
            apk_crash_image,
            1,
            ("LITEOS_APK_CRASH_RECOVERY_58",),
            interactions=(
                (
                    "Enter 'help' for a list of built-in commands.",
                    ShellCommand((
                        f"/sbin/apk --no-network add --upgrade /var/tmp/{apk_crash_v3}; "
                        "[ \"$(/bin/dd if=/usr/share/liteos-apk/crash bs=8 count=1 2>/dev/null)\" = crash-v3 ] && "
                        "/sbin/apk info -e liteos-apk-crash && "
                        "echo LITEOS_APK_CRASH_RECOVERY_$((6*9+4))\n"
                    ).encode()),
                ),
            ),
            forbidden_markers=FORBIDDEN_BOOT_MARKERS,
            persistent_writes=True,
        )
        crash_image = Path(runtime_directory.name) / "crash.img"
        shutil.copyfile(image, crash_image)
        install_runtime_execution_fixtures(
            crash_image,
            dynamic_probe,
            dynamic_library,
            runtime_path,
        )
        power_cut(
            crash_image,
            4,
            b"/bin/dynamic-smoke shared-crash\n",
            "LITEOS_SHARED_CRASH_ACTIVE_45",
            0.02,
        )
        power_cut(
            crash_image,
            4,
            b"/bin/rm -rf /crash; /bin/mkdir /crash; exec 3>/crash/open-unlinked; /bin/rm /crash/open-unlinked; echo LITEOS_ORPHAN_ACTIVE_43; while :; do echo payload >&3; done\n",
            "LITEOS_ORPHAN_ACTIVE_43",
            0.02,
        )
        crash_command = (
            b"/bin/rm -rf /crash; /bin/mkdir /crash; echo LITEOS_CRASH_LOOP_ACTIVE_43; "
            b"i=0; while :; do /bin/mkdir /crash/d; echo payload >/crash/d/source; /bin/chmod 0640 /crash/d/source; /bin/chown 1000:1000 /crash/d/source; "
            b"/bin/cp /crash/d/source /crash/d/copy; /bin/ln /crash/d/copy /crash/d/hard; "
            b"/bin/ln -s hard /crash/d/sym; /bin/mv /crash/d/copy /crash/d/moved; "
            b"/bin/rm -rf /crash/d; i=$((i+1)); done\n"
        )
        for delay in (0.0, 0.005, 0.02, 0.05):
            power_cut(
                crash_image,
                4,
                crash_command,
                "LITEOS_CRASH_LOOP_ACTIVE_43",
                delay,
            )
        boot(
            crash_image,
            1,
            ("LITEOS_JOURNAL_RECOVERY_43",),
            interactions=(
                (
                    "Enter 'help' for a list of built-in commands.",
                    ShellCommand(b"/bin/rm -rf /crash; /bin/mkdir /crash; echo recovered >/crash/state; sync; [ \"$(/bin/cat /crash/state)\" = recovered ] && echo LITEOS_JOURNAL_RECOVERY_$((6*7+1))\n"),
                ),
            ),
            forbidden_markers=FORBIDDEN_BOOT_MARKERS,
            persistent_writes=True,
        )
        e2fsck = next(
            (
                Path(candidate)
                for candidate in (
                    shutil.which("e2fsck"),
                    "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck",
                    "/usr/local/opt/e2fsprogs/sbin/e2fsck",
                    "/usr/sbin/e2fsck",
                )
                if candidate and Path(candidate).is_file()
            ),
            None,
        )
        if e2fsck is None:
            raise RuntimeError("e2fsck from e2fsprogs is required")
        check = subprocess.run(
            [str(e2fsck), "-fn", str(crash_image)],
            cwd=ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        if check.returncode != 0:
            raise RuntimeError(f"post-recovery e2fsck failed:\n{check.stdout}")
        publish_runtime_gate(stamp, payload)
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"BusyBox verification failed: {error}", file=sys.stderr)
        return 1
    finally:
        if https_server is not None:
            https_server.terminate()
            try:
                https_server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                https_server.kill()
                https_server.wait(timeout=3)
        if http_server is not None:
            http_server.terminate()
            try:
                http_server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                http_server.kill()
                http_server.wait(timeout=3)
        if runtime_directory is not None:
            runtime_directory.cleanup()
    print(f"BusyBox {BUSYBOX_VERSION} init+ash verification passed")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
