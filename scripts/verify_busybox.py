#!/usr/bin/env python3
"""构建固定上游 BusyBox 动态 PIE，并校验唯一受控配置与 ELF 边界。"""

from __future__ import annotations

import argparse
import io
import lzma
import shutil
import os
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path

from local_inputs import INPUTS

from apk_cache import cached_apk_bootstrap
from apk_rootfs import assemble_apk_rootfs, install_apk_crash_fixtures
from build_target import target_from_environment
from classic_assets import (
    boot_scene, cursors, fonts as classic_fonts, shell_bitmaps, sounds, wallpapers as classic_wallpapers,
)
from build_cache import (
    build_environment,
    build_jobs_override,
    cache_lock,
    fingerprint,
    generation_directory,
    make_command,
    manifest_matches,
    publish_directory,
    publish_generation,
    publish_runtime_gate,
    prune_rootfs_images,
    runtime_gate_hit,
    runtime_gate_payload,
    sha256,
    temporary_directory,
    write_manifest,
)
from qemu_gate import SHELL_PROMPT, boot, cpu_topology_markers, guest_inittab, power_cut, ShellCommand, TerminalInput
from openssl_cache import OpenSslPaths, build_openssl
from ext4_image import MKE2FS_CONFIG, debugfs_path, find_debugfs, find_mke2fs, verify_root_owned_files
from tls_gate import install_runtime_tls_identity, start_https_gate
from verify_musl import (
    MuslCachePaths,
    cached_musl_paths,
    compiler_identity,
    find_compiler,
    find_stripper,
    run,
)

ROOT = Path(__file__).resolve().parent.parent
TARGET = target_from_environment()
WORK = ROOT / "target" / "busybox-runtime" / TARGET.arch
SOURCE = INPUTS / "busybox" / TARGET.arch
# 主 BusyBox gate 以完整 kernel command line 冷启动：root/rootfstype/rootwait/console 由内核消费，
# LITEOS_BOOT_ENV 按 Linux 规则转交 init 环境并由 init 派生的 shell 继承，/proc/cmdline 原样回显。
# `root=LABEL=` 经超级块卷标解析根设备（不依赖设备枚举顺序）。
# `ro` 让根以只读挂载，由 rcS 的 `mount -o remount,rw /` 转为可写——同时覆盖该路径与 ext4 补做 orphan 回收。
BUSYBOX_KERNEL_ARGUMENTS = (
    "ro root=LABEL=LITEOS rootfstype=ext4 rootwait "
    f"console={'ttyAMA0' if TARGET.arch == 'aarch64' else 'ttyS0'} LITEOS_BOOT_ENV=42"
)
CONFIG_FRAGMENT = ROOT / "user" / "base" / "busybox.config"
BUSYBOX_VERSION = "1.37.0"
BUSYBOX_URLS = (
    "https://mirrors.aliyun.com/slackware/slackware64-current/source/"
    f"installer/sources/busybox/busybox-{BUSYBOX_VERSION}.tar.bz2",
    f"https://busybox.net/downloads/busybox-{BUSYBOX_VERSION}.tar.bz2",
)
BUSYBOX_SHA256 = "3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4"
SOURCE_RECIPE_VERSION = 1
BINARY_RECIPE_VERSION = 7
# OWNER: verify_busybox 唯一发布 content-addressed rootfs image cache。
# PROOF: 单一文件锁串行化 writer；create_image 完成全部 ownership 断言后才写 manifest，并把
# 完整 fingerprint directory 原子发布为不可变 generation。
# FAILURE: 缺少该 cache 会让 ext4 创建时间在每次 build 改写 fs.img，即使执行输入未变也会
# 使全部下游 APK install/runtime gate 失效。
ROOTFS_RECIPE_VERSION = 17
# APK assembly 同时保留已安装文件、压缩包及 APK 的原子替换临时文件。
# 容量按真实发布输入推导；固定最小值只覆盖小型 rootfs，不能限制新增运行库。
ROOTFS_MIN_SIZE_MIB = 512
RUST_USER_CARGO_CACHE_RECIPE_VERSION = 1
if TARGET.arch == "aarch64":
    BUSYBOX_ARCH = "arm64"
    BUSYBOX_TARGET_CFLAGS = "-march=armv8-a"
    ELF_MACHINE = "AArch64"
    RUST_USER_TARGET = "aarch64-unknown-linux-musl"
else:
    BUSYBOX_ARCH = "riscv"
    BUSYBOX_TARGET_CFLAGS = "-march=rv64gc -mabi=lp64d"
    ELF_MACHINE = "RISC-V"
    RUST_USER_TARGET = "riscv64gc-unknown-linux-musl"
RUST_USER_BUILD_STD = "std,panic_abort;llvm-libunwind"
# 系统共享库（对应 Windows 2000 的 DLL）与动态链接它们的程序；探针、session-launch 等其余程序整体静态。
# 运行时库承载 std，其余库与程序都链接它，所以全系统只有一份 std。Rust 没有稳定 ABI，库与程序必须由同一份
# 工具链、同一组参数、同一个 Cargo target 一次构建，并一起安装到 `/usr/lib/liteos`。
# 元素是 `(Cargo 包名, 库名)`，库文件名是 `lib<库名>.so`；顺序即依赖顺序。
RUNTIME_LIBRARY = "liteos_runtime"
UI_RUNTIME = ("classic-ui-runtime", "liteos_ui")
SYSTEM_LIBRARIES = (
    ("liteos-runtime", RUNTIME_LIBRARY), ("linux-uapi", "linux_uapi"),
    ("canvas", "canvas"), ("classic-theme", "classic_theme"), ("typeface", "typeface"),
    ("classic-ui", "classic_ui"), ("wayland-wire", "wayland_wire"),
    ("keyboard", "keyboard"), ("calendar", "calendar"), ("desktop-entry", "desktop_entry"),
    ("desktop-settings", "desktop_settings"),
    ("sound", "sound"), ("sndvol", "sndvol"),
    UI_RUNTIME,
)
SYSTEM_LIBRARY_DIRECTORY = "/usr/lib/liteos"
SYSTEM_LIBRARY_PROGRAMS = frozenset({
    ("compositor", "compositor"), ("logon", "logon"), ("desktop", "desktop"),
    ("sound-player", "sndplay"),
})
STABLE_UI_PROGRAMS = frozenset({("file-explorer", "file-explorer"), ("control-panel", "control-panel"), ("notepad", "notepad"), ("device-manager", "devmgmt"), ("volume-control", "sndvol"), ("terminal", "terminal"), ("task-manager", "taskmgr"), ("media-player", "mplay")})
RUST_USER_RUSTFLAGS = (
    "-C link-self-contained=no -C target-feature=-crt-static "
    "-C relocation-model=pic -C panic=abort "
    "-C link-arg=-Wl,--gc-sections,-z,relro,-z,now,-z,noexecstack"
)
# BusyBox 的 make-time `ARCH=arm64|riscv` 会覆盖 recipe 环境。CC/LD 必须在执行
# LiteOS wrapper 前恢复 canonical selector；缺少该边界时 wrapper 会把 BusyBox 的
# architecture spelling 当成公共 `ARCH` 并在编译第一项 target object 时 fail-stop。
BUSYBOX_CC = f"env ARCH={TARGET.arch} {sys.executable} {ROOT / 'scripts/musl_clang.py'}"
BUSYBOX_LD = f"env ARCH={TARGET.arch} {sys.executable} {ROOT / 'scripts/musl_ld.py'}"
FORBIDDEN_BOOT_MARKERS = (
    "Invalid argument",
    "init: can't log to /dev/tty5",
    "unsupported syscall_id:",
    "Please press Enter to activate this console.",
    "udhcpc:",
    "LITEOS_DNS_FAILURE",
    "\ufffd",
)
ACCOUNT_APPLETS = ("passwd", "adduser", "addgroup")
BUSYBOX_LINKS = (
    *ACCOUNT_APPLETS,
    "[",
    "[[",
    "arch",
    "ash",
    "awk",
    "basename",
    "busybox",
    "bunzip2",
    "bzip2",
    "bzcat",
    "cat",
    "chmod",
    "chown",
    "cksum",
    "clear",
    "cmp",
    "cp",
    "cut",
    "date",
    "dd",
    "df",
    "diff",
    "dirname",
    "du",
    "echo",
    "env",
    "expr",
    "false",
    "find",
    "free",
    "grep",
    "groups",
    "gunzip",
    "gzip",
    "halt",
    "hd",
    "head",
    "hexdump",
    "id",
    "ifconfig",
    "install",
    "kill",
    "killall",
    "ln",
    "ls",
    "less",
    "md5sum",
    "mkdir",
    "mktemp",
    "more",
    "mount",
    "mv",
    "nc",
    "netstat",
    "nohup",
    "od",
    "patch",
    "pgrep",
    "ping",
    "pidof",
    "pkill",
    "printf",
    "printenv",
    "poweroff",
    "ps",
    "pwd",
    "readlink",
    "realpath",
    "reboot",
    "reset",
    "rm",
    "rmdir",
    "route",
    "sed",
    "seq",
    "setsid",
    "sha1sum",
    "sha256sum",
    "sha512sum",
    "sh",
    "sleep",
    "sort",
    "stat",
    "strings",
    "stty",
    "sync",
    "tar",
    "tail",
    "tee",
    "test",
    "touch",
    "timeout",
    "top",
    "tr",
    "true",
    "tty",
    "umount",
    "uniq",
    "uname",
    "uptime",
    "udhcpc",
    "unxz",
    "unzip",
    "wc",
    "watch",
    "wget",
    "which",
    "whoami",
    "vi",
    "xargs",
    "xz",
    "xzcat",
    "yes",
    "zcat",
)
STRESS_LINKS = ("cputest", "memtest", "cachetest")


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


def source_payload() -> dict[str, object]:
    return {
        "kind": "busybox-source",
        "recipe_version": SOURCE_RECIPE_VERSION,
        "version": BUSYBOX_VERSION,
        "archive_sha256": BUSYBOX_SHA256,
        "strip_components": 1,
    }


def source_cache_path() -> Path:
    return SOURCE / "sources" / fingerprint(source_payload())


def obtain_source() -> Path:
    """获取并缓存固定源码；完整目录只在校验和解压成功后发布。"""
    SOURCE.mkdir(parents=True, exist_ok=True)
    archive = SOURCE / f"busybox-{BUSYBOX_VERSION}.tar.bz2"
    if not archive.is_file() or sha256(archive) != BUSYBOX_SHA256:
        archive.unlink(missing_ok=True)
        temporary = archive.with_suffix(".download")
        temporary.unlink(missing_ok=True)
        errors = []
        for url in BUSYBOX_URLS:
            print(f"downloading BusyBox {BUSYBOX_VERSION} from {url}")
            try:
                urllib.request.urlretrieve(url, temporary)
            except Exception as error:
                errors.append(f"{url}: {error}")
                temporary.unlink(missing_ok=True)
                continue
            if sha256(temporary) != BUSYBOX_SHA256:
                errors.append(f"{url}: SHA-256 mismatch")
                temporary.unlink(missing_ok=True)
                continue
            break
        else:
            temporary.unlink(missing_ok=True)
            raise RuntimeError(
                "failed to download BusyBox release tarball:\n" + "\n".join(errors)
            )
        temporary.replace(archive)

    payload = source_payload()
    source = source_cache_path()
    if manifest_matches(source, payload, ("Makefile", "scripts/kconfig/mconf.c")):
        return source

    temporary = temporary_directory(SOURCE / "sources", "source")
    try:
        run(
            ["tar", "-xjf", str(archive), "--strip-components=1", "-C", str(temporary)],
            ROOT,
        )
        write_manifest(temporary, payload)
        publish_directory(temporary, source)
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    return source


def fragment_assignments(path: Path) -> dict[str, str]:
    """读取显式赋值；生成配置中的其他 symbol 必须保持 allnoconfig 默认值。"""
    assignments: dict[str, str] = {}
    for raw_line in path.read_text().splitlines():
        line = raw_line.strip()
        if line.startswith("CONFIG_") and "=" in line:
            name = line.split("=", 1)[0]
        elif line.startswith("# CONFIG_") and line.endswith(" is not set"):
            name = line[2 : line.index(" is not set")]
        else:
            continue
        if name in assignments:
            raise RuntimeError(f"duplicate BusyBox config assignment: {name}")
        assignments[name] = line
    return assignments


def configure(source: Path, build: Path, env: dict[str, str]) -> None:
    """从全关闭状态应用唯一 fragment，避免 BusyBox 默认 applet 隐式进入产物。"""
    run(["make", "-C", str(source), f"O={build}", "allnoconfig"], ROOT, env)
    config = build / ".config"
    lines = config.read_text().splitlines()
    assignments = fragment_assignments(CONFIG_FRAGMENT)
    assignments["CONFIG_EXTRA_CFLAGS"] = (
        f'CONFIG_EXTRA_CFLAGS="{BUSYBOX_TARGET_CFLAGS} -DBB_GLOBAL_CONST="'
    )
    assignments["CONFIG_EXTRA_LDFLAGS"] = (
        f'CONFIG_EXTRA_LDFLAGS="{BUSYBOX_TARGET_CFLAGS} '
        '-Wl,-z,relro,-z,now,-z,noexecstack"'
    )
    replaced: set[str] = set()
    for index, line in enumerate(lines):
        if line.startswith("CONFIG_") and "=" in line:
            name = line.split("=", 1)[0]
        elif line.startswith("# CONFIG_") and line.endswith(" is not set"):
            name = line[2 : line.index(" is not set")]
        else:
            continue
        if name in assignments:
            lines[index] = assignments[name]
            replaced.add(name)
    missing = sorted(assignments.keys() - replaced)
    if missing:
        raise RuntimeError(f"BusyBox config contains unknown symbols: {', '.join(missing)}")
    config.write_text("\n".join(lines) + "\n")

    result = subprocess.run(
        ["make", "-C", str(source), f"O={build}", "oldconfig"],
        cwd=ROOT,
        env=env,
        input="\n" * 2048,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-80:])
        raise RuntimeError(f"BusyBox oldconfig failed\n{tail}")
    resolved = config.read_text().splitlines()
    resolved_set = set(resolved)
    drift = [line for line in assignments.values() if line not in resolved_set]
    if drift:
        raise RuntimeError(f"BusyBox rejected required config: {', '.join(drift)}")


def binary_payload(
    compiler: Path,
    musl: MuslCachePaths,
) -> dict[str, object]:
    stripper = find_stripper(compiler)
    return {
        "kind": "busybox-dynamic-pie",
        "recipe_version": BINARY_RECIPE_VERSION,
        "source_fingerprint": fingerprint(source_payload()),
        "config_sha256": sha256(CONFIG_FRAGMENT),
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "compiler": compiler_identity(compiler),
        "architecture": TARGET.arch,
        "linux_target": TARGET.linux_triple,
        "target_cflags": BUSYBOX_TARGET_CFLAGS,
        "drivers": {
            "compiler_sha256": sha256(ROOT / "scripts/musl_clang.py"),
            "linker_sha256": sha256(ROOT / "scripts/musl_ld.py"),
        },
        "strip": {
            "path": str(stripper),
            "sha256": sha256(stripper),
        },
        "environment": {
            "LC_ALL": "C",
            "CPATH": None,
            "C_INCLUDE_PATH": None,
            "CPLUS_INCLUDE_PATH": None,
            "LIBRARY_PATH": None,
        },
    }


def binary_cache_entry(
    compiler: Path,
    musl: MuslCachePaths,
) -> tuple[dict[str, object], str, Path]:
    payload = binary_payload(compiler, musl)
    binary_fingerprint = fingerprint(payload)
    binary = WORK / "binaries" / binary_fingerprint / "busybox"
    return payload, binary_fingerprint, binary


def cached_busybox_binary(compiler: Path) -> Path:
    """返回 fingerprint 与 manifest 均匹配的当前 BusyBox ELF。"""
    musl = cached_musl_paths(compiler)
    payload, _, binary = binary_cache_entry(compiler, musl)
    if not manifest_matches(binary.parent, payload, ("busybox", "busybox_unstripped")):
        raise RuntimeError("BusyBox binary cache is missing; run verify_busybox.py first")
    return binary.resolve()


def build_busybox(
    source: Path,
    compiler: Path,
    jobs_override: int | None,
    rebuild: bool = False,
) -> Path:
    """按 source/config/musl/toolchain fingerprint 构建或复用动态 BusyBox。"""
    musl = cached_musl_paths(compiler)
    env = build_environment()
    payload, binary_fingerprint, binary = binary_cache_entry(compiler, musl)
    if not rebuild and manifest_matches(binary.parent, payload, ("busybox", "busybox_unstripped")):
        print(f"BusyBox binary cache hit: {binary_fingerprint[:12]}")
        return binary.resolve()

    build = temporary_directory(WORK / "builds", "build")
    generation = generation_directory(WORK / "binary-generations", binary_fingerprint)
    published = False
    try:
        # 1. 使用 BusyBox 原生 O= 隔离机制，immutable source 始终只读。
        # 2. configure/build 全部发生在私有输出树，并发 reader 不会观察中间状态。
        # 3. 仅复制最终 ELF 到 generation，manifest 完整后才原子发布。
        configure(source, build, env)
        env.update({
            "LITEOS_MUSL_CLANG": str(musl.compiler),
            "LITEOS_MUSL_LLD": str(musl.linker),
            "LITEOS_MUSL_COMPILER_RUNTIME": str(musl.compiler_runtime),
            "LITEOS_MUSL_SYSROOT": str(musl.install),
        })
        run(
            [
                *make_command(jobs_override),
                "-C",
                str(source),
                f"O={build}",
                f"ARCH={BUSYBOX_ARCH}",
                f"CC={BUSYBOX_CC}",
                f"LD={BUSYBOX_LD}",
                f"AR={musl.archiver}",
                f"STRIP={payload['strip']['path']}",
            ],
            ROOT,
            env,
        )
        built_binary = build / "busybox"
        if not built_binary.is_file():
            raise RuntimeError("BusyBox build did not produce busybox")
        shutil.copy2(built_binary, generation / "busybox")
        shutil.copy2(build / "busybox_unstripped", generation / "busybox_unstripped")
        write_manifest(generation, payload)
        publish_generation(generation, binary.parent)
        published = True
    finally:
        shutil.rmtree(build, ignore_errors=True)
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    print(f"BusyBox binary cache populated: {binary_fingerprint[:12]}")
    return binary.resolve()


def verify_elf(
    binary: Path,
    compiler: Path,
    required_libraries: tuple[str, ...] = ("libc.so",),
    identity: str = "BusyBox",
) -> None:
    """要求目标架构动态 PIE、标准 musl interpreter、RELRO、W^X 与 NX stack。"""
    prefix = str(compiler)[: -len("gcc")]
    readelf = Path(f"{prefix}readelf")
    if not readelf.is_file():
        candidate = shutil.which("llvm-readelf") or "/opt/homebrew/opt/llvm/bin/llvm-readelf"
        readelf = Path(candidate)
    if not readelf.is_file():
        raise RuntimeError(f"{TARGET.arch} readelf or llvm-readelf is required")
    output = run(
        [str(readelf), "--file-header", "--program-headers", "--dynamic", "--wide", str(binary)], ROOT
    )
    for marker in ("ELF64", ELF_MACHINE, "DYN ("):
        if marker not in output:
            raise RuntimeError(f"{identity} ELF lacks {marker!r}")
    headers = [line.split() for line in output.splitlines()]
    if (
        output.count("Requesting program interpreter:") != 1
        or TARGET.musl_loader not in output
    ):
        raise RuntimeError(
            f"{identity} must use the standard {TARGET.arch} musl interpreter"
        )
    for marker in ("DYNAMIC", "GNU_RELRO", "NOW PIE"):
        if marker not in output:
            raise RuntimeError(f"{identity} dynamic ELF lacks {marker!r}")
    needed = {
        line.split("Shared library: [", 1)[1].split("]", 1)[0]
        for line in output.splitlines()
        if "Shared library: [" in line
    }
    # 必需库必须出现；动态程序还可以依赖其他系统共享库，但不能依赖这之外的任何库。
    # SDK 应用只依赖稳定运行库和 musl；不能悄悄重新引入系统 Rust metadata 边界。
    allowed = set(required_libraries)
    if "libliteos_ui.so.1" not in required_libraries:
        allowed |= {"libc.so"} | {f"lib{name}.so" for _, name in SYSTEM_LIBRARIES}
    if not set(required_libraries) <= needed or not needed <= allowed:
        raise RuntimeError(
            f"{identity} ELF dependencies differ: expected {sorted(required_libraries)}, got {sorted(needed)}"
        )
    if "TEXTREL" in output:
        raise RuntimeError(f"{identity} dynamic ELF contains text relocations")
    loads = [columns for columns in headers if columns and columns[0] == "LOAD"]
    if not loads or not any(int(columns[1], 16) == 0 for columns in loads):
        raise RuntimeError(f"{identity} PIE PHDR table is not covered by an offset-zero LOAD")
    for columns in headers:
        if len(columns) < 8 or columns[0] not in {"LOAD", "GNU_STACK"}:
            continue
        flags = "".join(columns[6:-1])
        if columns[0] == "LOAD" and "W" in flags and "E" in flags:
            raise RuntimeError(f"{identity} contains a writable executable LOAD")
        if columns[0] == "GNU_STACK" and "E" in flags:
            raise RuntimeError(f"{identity} requests an executable stack")


def verify_user_artifact(
    path: Path, compiler: Path, required_libraries: tuple[str, ...], identity: str, library: bool
) -> None:
    """程序按动态 PIE 校验；系统库按共享对象校验（没有 interpreter，也不是 PIE 入口）。"""
    if not library:
        verify_elf(path, compiler, required_libraries, identity)
        return
    prefix = str(compiler)[: -len("gcc")]
    readelf = Path(f"{prefix}readelf")
    if not readelf.is_file():
        readelf = Path(shutil.which("llvm-readelf") or "/opt/homebrew/opt/llvm/bin/llvm-readelf")
    output = run([str(readelf), "--file-header", "--program-headers", "--dynamic", "--wide", str(path)], ROOT)
    for marker in ("ELF64", ELF_MACHINE, "DYN (", "GNU_RELRO"):
        if marker not in output:
            raise RuntimeError(f"{identity} shared library lacks {marker!r}")
    needed = {
        line.split("Shared library: [", 1)[1].split("]", 1)[0]
        for line in output.splitlines()
        if "Shared library: [" in line
    }
    allowed = set(required_libraries)
    if identity != f"lib{UI_RUNTIME[1]}.so":
        allowed |= {"libc.so"} | {f"lib{name}.so" for _, name in SYSTEM_LIBRARIES}
    if not set(required_libraries) <= needed or not needed <= allowed:
        raise RuntimeError(f"{identity} dependencies differ: expected {sorted(required_libraries)}, got {sorted(needed)}")
    if "TEXTREL" in output or "Requesting program interpreter" in output:
        raise RuntimeError(f"{identity} must be a position-independent shared object")
    for line in output.splitlines():
        columns = line.split()
        if len(columns) >= 8 and columns[0] == "GNU_STACK" and "E" in "".join(columns[6:-1]):
            raise RuntimeError(f"{identity} requests an executable stack")


def build_system_libraries(musl: MuslCachePaths) -> list[tuple[Path, str]]:
    """构建全部系统共享库，返回 `(宿主文件, 安装文件名)`；与动态程序共享同一个 Cargo target。"""
    libraries = [
        (build_rust_user_program(musl, package, name, f"system-library-{name}", 1, library=True), f"lib{name}.so")
        for package, name in SYSTEM_LIBRARIES
    ]
    # SDK 链接名与运行时 SONAME 同时安装；旧进程保留旧 inode，新进程解析 ABI 主版本。
    ui = next(path for path, name in libraries if name == "libliteos_ui.so")
    return [*libraries, (ui, "libliteos_ui.so.1")]


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


def rust_user_cargo_target(
    musl: MuslCachePaths,
    cargo_version: str,
    rustc_version: str,
    driver_sha256: str,
    libunwind_sha256: str,
    dynamic: bool = False,
) -> Path:
    """返回只绑定编译环境、不绑定源码 revision 的持久 Cargo cache。

    Args:
        musl: 当前目标架构的 musl sysroot 身份。
        cargo_version: 实际执行的 Cargo 版本字符串。
        rustc_version: 实际执行的 rustc 版本字符串。
        driver_sha256: 唯一 musl linker driver 的内容摘要。
        libunwind_sha256: 当前静态 libunwind 的内容摘要。

    Returns:
        由完整编译环境身份隔离的 mutable Cargo target 目录。
    """
    compilation_payload = {
        "kind": "rust-user-cargo-target",
        "dynamic": dynamic,
        "recipe_version": RUST_USER_CARGO_CACHE_RECIPE_VERSION,
        "arch": TARGET.arch,
        "rust_target": RUST_USER_TARGET,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "driver_sha256": driver_sha256,
        "cargo": cargo_version,
        "rustc": rustc_version,
        "build_std": RUST_USER_BUILD_STD,
        "libunwind_sha256": libunwind_sha256,
        "rustflags": f"{RUST_USER_RUSTFLAGS} -L native=<libunwind> -D warnings",
    }
    return WORK / "rust-user-cargo-targets" / fingerprint(compilation_payload)


def build_rust_user_program(
    musl: MuslCachePaths,
    crate_name: str,
    binary_name: str,
    kind: str,
    recipe_version: int,
    *,
    library: bool = False,
) -> Path:
    """经唯一 Cargo→musl PIE 路径构建 Rust userspace 程序或系统共享库（`SYSTEM_LIBRARIES`）。

    内部 Rust dylib 消费者用 prefer-dynamic；稳定 UI 运行库和 SDK 应用静态纳入自己的 Rust/std，
    仅通过 libliteos_ui.so.1 与 musl 交互。所有产物仍由同一发布/ELF 校验路径安装。
    """
    # 稳定 UI 边界必须静态纳入自身 Rust/std，不能让 SDK 间接依赖系统 Rust dylib metadata。
    stable_ui = library and (crate_name, binary_name) == UI_RUNTIME
    sdk_consumer = not library and (crate_name, binary_name) in STABLE_UI_PROGRAMS
    ui_library = build_rust_user_program(musl, *UI_RUNTIME, "system-library-liteos_ui", 1, library=True) if sdk_consumer else None
    dynamic = (library and not stable_ui) or (crate_name, binary_name) in SYSTEM_LIBRARY_PROGRAMS
    if library and (crate_name, binary_name) not in SYSTEM_LIBRARIES:
        raise RuntimeError(f"{crate_name} is not a registered system library")
    artifact = f"lib{binary_name}.so" if library else binary_name
    workspace = ROOT / "user"
    sources = tuple(sorted(workspace.rglob("*.rs")))
    cargo = shutil.which("cargo")
    rustc = shutil.which("rustc")
    if cargo is None or rustc is None:
        raise RuntimeError(f"nightly Cargo and rustc are required for {crate_name}")
    available_targets = set(run([rustc, "--print", "target-list"], ROOT).splitlines())
    if RUST_USER_TARGET not in available_targets:
        raise RuntimeError(
            f"rustc does not provide required {TARGET.arch} userspace target "
            f"{RUST_USER_TARGET}; refusing to reuse another architecture"
        )
    cargo_version = run([cargo, "--version"], ROOT).strip()
    rustc_version = run([rustc, "--version"], ROOT).strip()
    payload = {
        "kind": kind,
        "dynamic": dynamic,
        "ui_runtime_sha256": sha256(ui_library) if ui_library else None,
        "recipe_version": recipe_version,
        "arch": TARGET.arch,
        "rust_target": RUST_USER_TARGET,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "driver_sha256": sha256(ROOT / "scripts/musl_clang.py"),
        "cargo": cargo_version,
        "rustc": rustc_version,
        "build_std": RUST_USER_BUILD_STD,
        "manifest_sha256": {
            str(path.relative_to(workspace)): sha256(path)
            for path in sorted(workspace.glob("*/*/Cargo.toml"))
        }
        | {"Cargo.toml": sha256(workspace / "Cargo.toml")},
        "lock_sha256": sha256(workspace / "Cargo.lock"),
        "classic_manifest_sha256": sha256(ROOT / "assets/classic/manifest.json"),
        "smb_vendor_sha256": {
            str(path.relative_to(ROOT)): sha256(path)
            for name in ("smb", "smb-dtyp", "smb-fscc")
            for path in sorted((ROOT / "vendor" / name).rglob("*")) if path.is_file()
        },
        "source_sha256": {
            str(source.relative_to(workspace)): sha256(source)
            for source in sources
        },
    }
    # 延迟导入打破 gate 编排模块间的初始化环；libunwind 构建本身由独立
    # content-addressed cache 持有。
    from verify_rust_std import build_libunwind

    libunwind = build_libunwind(musl, rustc)
    payload["libunwind_sha256"] = sha256(libunwind)
    # OWNER: verify_busybox 在 WORK/.build.lock 下独占此 mutable Cargo cache。
    # PROOF: toolchain、target、sysroot、libunwind 与完整 flags 都进入目录身份；最终 ELF 仍复制到
    # 独立的 content-addressed generation，校验并发布 manifest 后才可被 rootfs 消费。
    # FAILURE: 缺少持久目录只会让每次源码变化重复构建 std/依赖，不会改变发布产物身份。
    cargo_target = rust_user_cargo_target(
        musl,
        cargo_version,
        rustc_version,
        str(payload["driver_sha256"]),
        str(payload["libunwind_sha256"]),
        dynamic,
    )
    entry = WORK / "rust-user-programs" / fingerprint(payload)
    required_libraries = ("libc.so", "libliteos_ui.so.1") if sdk_consumer else (
        (f"lib{RUNTIME_LIBRARY}.so",)
        if dynamic and binary_name != RUNTIME_LIBRARY
        else ("libc.so",)
    )
    if manifest_matches(entry, payload, (artifact,)):
        verify_user_artifact(entry / artifact, musl.compiler, required_libraries, artifact, library)
        return entry / artifact
    generation = generation_directory(WORK / "rust-user-program-generations", fingerprint(payload))
    env = build_environment()
    linker_variable = f"CARGO_TARGET_{RUST_USER_TARGET.upper().replace('-', '_')}_LINKER"
    env.update(
        {
            "LITEOS_MUSL_CLANG": str(musl.compiler),
            "LITEOS_MUSL_LLD": str(musl.linker),
            "LITEOS_MUSL_COMPILER_RUNTIME": str(musl.compiler_runtime),
            "LITEOS_MUSL_SYSROOT": str(musl.install),
            "LITEOS_RUST_PROVIDES_COMPILER_BUILTINS": "1",
            "CARGO_INCREMENTAL": "0",
            "CARGO_TARGET_DIR": str(cargo_target),
            linker_variable: str(ROOT / "scripts/musl_clang.py"),
            "RUSTFLAGS": (
                f"{RUST_USER_RUSTFLAGS} "
                f"{'-C prefer-dynamic ' if dynamic else ''}"
                f"-L native={libunwind.parent} "
                f"{'-L native=' + str(ui_library.parent) + ' ' if ui_library else ''}-D warnings"
            ),
        }
    )
    if dynamic:
        # LTO 与 `prefer-dynamic` 互斥；动态程序本身很小，系统库跨 crate 优化损失可以接受。
        env["CARGO_PROFILE_RELEASE_LTO"] = "off"
    published = False
    try:
        run(
            [
                cargo,
                "build",
                "-Z",
                "build-std=std,panic_abort",
                "-Z",
                "build-std-features=llvm-libunwind",
                "--manifest-path",
                str(workspace / "Cargo.toml"),
                "-p",
                crate_name,
                *(["--lib"] if library else ["--bin", binary_name]),
                "--target",
                RUST_USER_TARGET,
                "--release",
                "--locked",
            ],
            ROOT,
            env,
        )
        built = (
            cargo_target
            / RUST_USER_TARGET
            / "release"
            / artifact
        )
        if not built.is_file():
            raise RuntimeError(f"Cargo did not produce {artifact}")
        shutil.copy2(built, generation / artifact)
        verify_user_artifact(
            generation / artifact,
            musl.compiler,
            required_libraries,
            artifact,
            library,
        )
        write_manifest(generation, payload)
        publish_generation(generation, entry)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    return entry / artifact


def build_drm_flip_probe(musl: MuslCachePaths) -> Path:
    """构建内核显示/GPU 门禁使用的 DRM page-flip 探针；它不进入产品 rootfs。"""
    return build_rust_user_program(
        musl,
        "linux-uapi",
        "drm-flip-probe",
        "drm-flip-probe",
        1,
    )


@dataclass(frozen=True)
class UserlandArtifact:
    """一个安装到开发/rootfs 镜像的系统文件。

    Attributes:
        source: 已构建并校验的 host 文件。
        destination: 文件在 guest 中的绝对路径。
        mode: regular file 的 Unix permission bits。
    """

    source: Path
    destination: str
    mode: int = 0o644


def base_userland(musl: MuslCachePaths) -> tuple[UserlandArtifact, ...]:
    """构建产品系统文件的唯一安装清单；构建与开发镜像同步共同消费。"""
    from ui_sdk import sdk_files
    from mesa_runtime import mesa_userland
    from fuse_runtime import fuse_userland

    programs = tuple(
        UserlandArtifact(build_rust_user_program(musl, crate, binary, binary, 1), f"/bin/{binary}", 0o755)
        for crate, binary in (("compositor", "compositor"), ("logon", "logon"),
                              ("desktop", "desktop"), ("file-explorer", "file-explorer"), ("smbfs", "smbfs"),
                              ("control-panel", "control-panel"), ("task-manager", "taskmgr"), ("volume-control", "sndvol"),
                              ("sound-player", "sndplay"), ("media-player", "mplay"),
                              ("notepad", "notepad"), ("device-manager", "devmgmt"),
                              ("linux-uapi", "session-launch"), ("terminal", "terminal"))
    )
    fonts = tuple(UserlandArtifact(path, f"/usr/share/liteos/fonts/{path.name}")
                  for path in classic_fonts())
    ui = tuple(UserlandArtifact(path, f"/usr/share/liteos/ui/{path.name}")
               for path in shell_bitmaps())
    # Windows 2000 把墙纸装在 %windir%；LiteOS 的系统墙纸目录保留原安装名，显示属性按文件名列出。
    wallpapers = tuple(UserlandArtifact(path, f"/usr/share/liteos/wallpapers/{path.name}")
                       for path in classic_wallpapers())
    return (
        UserlandArtifact(ROOT / "user/base/shutdown", "/bin/shutdown", 0o755),
        UserlandArtifact(ROOT / "user/base/rcS", "/etc/init.d/rcS", 0o755),
        UserlandArtifact(ROOT / "user/base/inittab", "/etc/inittab"),
        UserlandArtifact(ROOT / "user/base/profile", "/etc/profile"),
        UserlandArtifact(boot_scene(), "/usr/share/liteos/boot-scene.lbs"),
        UserlandArtifact(cursors(), "/usr/share/liteos/cursors.lcs"),
        UserlandArtifact(ROOT / "assets/fonts/liteos-terminal.a8", "/usr/share/liteos/liteos-terminal.a8"),
        UserlandArtifact(ROOT / "user/base/display-service", "/etc/init.d/display-service", 0o755),
        UserlandArtifact(ROOT / "user/base/network-service", "/etc/init.d/network-service", 0o755),
        UserlandArtifact(ROOT / "user/base/udhcpc.script", "/usr/share/udhcpc/default.script", 0o755),
        # 桌面条目：user/base 下的每个 .desktop 都安装；窗口图标、文件关联与控制面板入口都由它们声明。
        *(UserlandArtifact(path, f"/usr/share/applications/{path.name}")
          for path in sorted((ROOT / "user/base").glob("*.desktop"))),
        *(UserlandArtifact(path, f"{SYSTEM_LIBRARY_DIRECTORY}/{name}", 0o755)
          for path, name in build_system_libraries(musl)),
        UserlandArtifact(ROOT / "user/base/ld-musl.path", f"/etc/{Path(TARGET.musl_loader).name.removesuffix('.so.1')}.path"),
        *programs, *fonts, *ui, *wallpapers, *mesa_userland(), *fuse_userland(),
        *(UserlandArtifact(path, destination, mode) for path, destination, mode in sdk_files()),
        *(UserlandArtifact(path, f"/usr/share/liteos/sounds/{name}") for name, path in sounds()),
    )


def musl_development_commands(install: Path) -> list[str]:
    """Returns debugfs commands installing the musl headers, startup objects and static archives.

    `liteos-base` provides `musl-dev` with these files, so Alpine packages that depend on `musl-dev`
    (rust, gcc) resolve against the musl actually shipped here instead of pulling Alpine's own musl,
    which would conflict with the file and `so:libc.musl` ownership of `liteos-base`.
    """
    commands: list[str] = []
    include = install / "usr/include"
    directories = sorted({Path("/usr/include"), *(
        Path("/usr/include") / path.relative_to(include)
        for path in include.rglob("*")
        if path.is_dir()
    )})
    commands.extend(f"mkdir {directory}" for directory in directories)
    files = [
        (path, Path("/usr/include") / path.relative_to(include))
        for path in sorted(include.rglob("*"))
        if path.is_file()
    ]
    library = install / "usr/lib"
    files.extend(
        (path, Path("/usr/lib") / path.name)
        for path in sorted(library.iterdir())
        if path.suffix in (".a", ".o")
    )
    for source, destination in files:
        commands.append(f"write {source} {destination}")
        commands.append(f"set_inode_field {destination} mode 0100644")
    return commands


def build_stress_tools(musl: MuslCachePaths) -> Path:
    """构建 rootfs 单一 CPU/memory/page-cache 诊断程序。"""
    source = ROOT / "user/diagnostics/liteos-stress.c"
    payload = {
        "kind": "liteos-stress-tools",
        "recipe_version": 1,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "driver_sha256": sha256(ROOT / "scripts/musl_clang.py"),
        "source_sha256": sha256(source),
    }
    entry = WORK / "stress-tools" / fingerprint(payload)
    if manifest_matches(entry, payload, ("liteos-stress",)):
        return entry / "liteos-stress"
    generation = generation_directory(WORK / "stress-tool-generations", fingerprint(payload))
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
                "-std=c11",
                "-D_GNU_SOURCE",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-fPIE",
                "-pie",
                "-pthread",
                "-o",
                str(generation / "liteos-stress"),
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
    return entry / "liteos-stress"


def create_image(
    binary: Path,
    musl: MuslCachePaths,
    openssl: OpenSslPaths,
    image: Path,
) -> Path:
    """构造 BusyBox、唯一 musl runtime、标准 loader symlink 与固定 inittab。"""
    userland = base_userland(musl)
    stress_tools = build_stress_tools(musl)
    payload_bytes = sum(artifact.source.stat().st_size for artifact in userland)
    payload_bytes += sum(path.stat().st_size for path in (binary, openssl.binary, stress_tools))
    payload_bytes += sum(path.stat().st_size for path in (musl.install / "usr").rglob("*") if path.is_file())
    # 1. 现有树与压缩包同时存在；2. apk 在原文件旁创建替换文件；3. 另留元数据和块对齐空间。
    # 三份未压缩输入是保守上界，64 MiB 对齐保持 sparse image，避免依赖压缩率而在安装中 ENOSPC。
    size_mib = max(ROOTFS_MIN_SIZE_MIB, ((3 * payload_bytes + 64 * 1024 * 1024 + (64 * 1024 * 1024 - 1)) // (64 * 1024 * 1024)) * 64)
    run(
        [
            sys.executable,
            "create_fs.py",
            "create",
            "--file",
            str(image),
            "--size",
            str(size_mib),
            "--init",
            str(binary),
        ],
        ROOT,
    )
    bootstrap = cached_apk_bootstrap()
    commands = [
        "mkdir /etc",
        "mkdir /etc/init.d",
        "mkdir /etc/ssl",
        "mkdir /lib",
        "mkdir /run",
        "mkdir /root",
        "set_inode_field /root mode 040700",
        "mkdir /root/Documents",
        "mkdir /root/Downloads",
        "mkdir /root/Music",
        "mkdir /root/Pictures",
        "mkdir /root/Videos",
        "mkdir /tmp",
        "set_inode_field /tmp mode 041777",
        "mkdir /usr",
        "mkdir /usr/bin",
        "mkdir /usr/lib",
        "mkdir /usr/lib/liteos",
        "mkdir /usr/share",
        "mkdir /usr/share/liteos",
        "mkdir /usr/share/liteos/fonts",
        "mkdir /usr/share/liteos/ui",
        "mkdir /usr/share/liteos/sounds",
        "mkdir /usr/share/applications",
        "mkdir /home",
        "mkdir /usr/share/udhcpc",
        "mkdir /etc/network",
        "mkdir /var",
        "mkdir /var/cache",
        "mkdir /var/empty",
        "mkdir /var/lib",
        "mkdir /var/tmp",
        "set_inode_field /var/tmp mode 041777",
        f"write {ROOT / 'user' / 'base' / 'passwd'} /etc/passwd",
        f"write {ROOT / 'user' / 'base' / 'group'} /etc/group",
        f"write {ROOT / 'user' / 'base' / 'shadow'} /etc/shadow",
        "set_inode_field /etc/shadow mode 0100600",
        # 默认时区（UTC+8）；之后由控制面板改写，同步器只在缺失时补上。
        f"write {ROOT / 'user' / 'base' / 'localtime'} /etc/localtime",
        # 默认网络配置（eth0 DHCP）；之后由控制面板改写，同步器只在缺失时补上。
        f"write {ROOT / 'user' / 'base' / 'interfaces'} /etc/network/interfaces",
        f"write {openssl.binary} /bin/openssl",
        "set_inode_field /bin/openssl mode 0100755",
        f"write {musl.install / 'usr/lib/libc.so'} /usr/lib/libc.so",
        "set_inode_field /usr/lib/libc.so mode 0100755",
        f"write {stress_tools} /bin/liteos-stress",
        "set_inode_field /bin/liteos-stress mode 0100755",
        "ln /bin/liteos-stress /bin/cputest",
        "ln /bin/liteos-stress /bin/memtest",
        "ln /bin/liteos-stress /bin/cachetest",
        f"set_inode_field /bin/liteos-stress links_count {len(STRESS_LINKS) + 1}",
        f"symlink {TARGET.musl_loader} /usr/lib/libc.so",
        *musl_development_commands(musl.install),
    ]
    # 安装清单同时决定目录树：SDK/Mesa 等新增嵌套路径不能依赖另一份手写 mkdir 列表。
    existing_directories = {command.removeprefix("mkdir ") for command in commands if command.startswith("mkdir ")}
    artifact_directories = {str(parent) for artifact in userland for parent in Path(artifact.destination).parents if str(parent) != "/"}
    commands.extend(f"mkdir {debugfs_path(folder)}" for folder in sorted(artifact_directories - existing_directories, key=lambda path: (path.count("/"), path)))
    # 启动配置与共享开机素材只有 base_userland 一份安装清单。
    for artifact in userland:
        commands.append(f"write {debugfs_path(artifact.source)} {debugfs_path(artifact.destination)}")
        commands.append(
            f"set_inode_field {debugfs_path(artifact.destination)} mode 0{0o100000 | artifact.mode:o}"
        )
    commands.extend(f"ln /bin/init /bin/{applet}" for applet in BUSYBOX_LINKS)
    # npm/pnpm use `#!/usr/bin/env node`; without the standard env location apk installs succeed
    # but every JavaScript package-manager entry fails exec with ENOENT.
    commands.append("ln /bin/init /usr/bin/env")
    commands.append(f"set_inode_field /bin/init links_count {len(BUSYBOX_LINKS) + 2}")
    script_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile("w", delete=False) as script:
            script.write("\n".join(commands) + "\n")
            script_path = Path(script.name)
        run([str(find_debugfs()), "-w", "-f", str(script_path), str(image)], ROOT)
    finally:
        if script_path is not None:
            script_path.unlink(missing_ok=True)
    with tempfile.TemporaryDirectory(prefix="liteos-apk-rootfs-") as workspace:
        assemble_apk_rootfs(
            image,
            find_debugfs(),
            Path(workspace),
            BUSYBOX_LINKS,
            STRESS_LINKS,
            FORBIDDEN_BOOT_MARKERS,
            (),
        )
    listing = run([str(find_debugfs()), "-R", "ls -l /bin", str(image)], ROOT)
    entries: dict[str, int] = {}
    for line in listing.splitlines():
        fields = line.split()
        if len(fields) >= 9 and fields[0].isdigit():
            entries[fields[-1]] = int(fields[0])
    expected = {"init", *BUSYBOX_LINKS}
    missing = sorted(expected - entries.keys())
    if missing:
        raise RuntimeError(f"BusyBox rootfs lacks applets: {', '.join(missing)}")
    if len({entries[name] for name in expected}) != 1:
        raise RuntimeError("BusyBox applets must be hardlinks to one inode")
    metadata = run([str(find_debugfs()), "-R", "stat /bin/init", str(image)], ROOT)
    if f"Links: {len(expected) + 1}" not in metadata:
        raise RuntimeError("BusyBox inode link count does not match rootfs applets")
    standard_env = run(
        [str(find_debugfs()), "-R", "stat /usr/bin/env", str(image)], ROOT
    )
    if f"Inode: {entries['init']}" not in standard_env or "Mode:  0755" not in standard_env:
        raise RuntimeError("BusyBox rootfs lacks the standard /usr/bin/env hardlink")
    stress_commands = {"liteos-stress", *STRESS_LINKS}
    missing_stress = sorted(stress_commands - entries.keys())
    if missing_stress:
        raise RuntimeError(f"rootfs lacks stress commands: {', '.join(missing_stress)}")
    if len({entries[name] for name in stress_commands}) != 1:
        raise RuntimeError("stress commands must be hardlinks to one inode")
    stress_metadata = run(
        [str(find_debugfs()), "-R", "stat /bin/liteos-stress", str(image)], ROOT
    )
    if "Links: 4" not in stress_metadata:
        raise RuntimeError("stress command inode link count does not match multicall names")
    temporary_directory_metadata = run(
        [str(find_debugfs()), "-R", "stat /tmp", str(image)], ROOT
    )
    if "Mode:  01777" not in temporary_directory_metadata:
        raise RuntimeError("BusyBox rootfs /tmp must have mode 01777")
    passwd = run([str(find_debugfs()), "-R", "cat /etc/passwd", str(image)], ROOT)
    group = run([str(find_debugfs()), "-R", "cat /etc/group", str(image)], ROOT)
    if "root:x:0:0:root:/root:/bin/sh" not in passwd or "root:x:0:" not in group:
        raise RuntimeError("BusyBox rootfs lacks the canonical root identity records")
    for fixture in ("dynamic-smoke", "liteos-script", "rust-std-smoke"):
        if fixture in entries:
            raise RuntimeError(f"product rootfs contains verification fixture: {fixture}")
    product_libraries = run(
        [str(find_debugfs()), "-R", "ls -l /usr/lib", str(image)], ROOT
    )
    if "libliteos-smoke.so" in product_libraries:
        raise RuntimeError("product rootfs contains verification fixture: libliteos-smoke.so")
    loader = run(
        [str(find_debugfs()), "-R", f"stat {TARGET.musl_loader}", str(image)],
        ROOT,
    )
    if "Type: symlink" not in loader or "Size: 16" not in loader:
        raise RuntimeError("BusyBox rootfs lacks the standard musl loader symlink")
    network_service = run(
        [str(find_debugfs()), "-R", "stat /etc/init.d/network-service", str(image)], ROOT
    )
    if "Type: regular" not in network_service or "Mode:  0755" not in network_service:
        raise RuntimeError("BusyBox rootfs lacks the supervised network service")
    openssl_binary = run([str(find_debugfs()), "-R", "stat /bin/openssl", str(image)], ROOT)
    if "Type: regular" not in openssl_binary or "Mode:  0755" not in openssl_binary:
        raise RuntimeError("BusyBox rootfs lacks the verified HTTPS helper")
    with tempfile.TemporaryDirectory(prefix="liteos-base-readback-") as directory:
        for index, artifact in enumerate(userland):
            observed = Path(directory) / str(index)
            run([str(find_debugfs()), "-R", f"dump {debugfs_path(artifact.destination)} {observed}", str(image)], ROOT)
            if not observed.is_file() or sha256(observed) != sha256(artifact.source):
                raise RuntimeError(f"BusyBox rootfs system file differs: {artifact.destination}")
    verify_root_owned_files(image, tuple((artifact.destination, artifact.mode) for artifact in userland))
    ca_bundle = run([str(find_debugfs()), "-R", "stat /etc/ssl/cert.pem", str(image)], ROOT)
    ca_store = run(
        [str(find_debugfs()), "-R", "stat /etc/ssl/certs/ca-certificates.crt", str(image)],
        ROOT,
    )
    if "Type: symlink" not in ca_bundle or "Type: regular" not in ca_store:
        raise RuntimeError("BusyBox rootfs lacks the package-owned Mozilla CA trust bundle")
    return image


def target_runtime_artifacts() -> tuple[Path, ...]:
    """返回 rootfs 与 runtime gate 必须绑定的目标 release 产物。"""
    artifacts = [ROOT / f"target/{TARGET.kernel_triple}/release/kernel"]
    if TARGET.requires_bootloader:
        artifacts.append(
            ROOT / f"bootloader/target/{TARGET.kernel_triple}/release/bootloader"
        )
    return tuple(artifacts)


def create_published_image(
    binary: Path,
    musl: MuslCachePaths,
    openssl: OpenSslPaths,
    output: Path,
) -> Path:
    """从内容寻址缓存原子发布一份可写 rootfs。

    Args:
        binary: 已校验的 BusyBox ELF。
        musl: 已校验的 musl sysroot 与工具链 identity。
        openssl: 已校验的 target OpenSSL 产物。
        output: 调用方独占的可写镜像路径。

    Returns:
        已原子替换为完整缓存镜像的 output。

    Raises:
        RuntimeError: 构建工具、APK bootstrap 或 rootfs assembly 失败。
        OSError: cache publication 或 output copy 失败。
    """
    userland = base_userland(musl)
    stress_tools = build_stress_tools(musl)
    bootstrap = cached_apk_bootstrap()
    host_openssl = shutil.which("openssl")
    if host_openssl is None:
        raise RuntimeError("host OpenSSL is required to sign the rootfs package")
    alpine_keys = tuple(
        sorted(path for path in bootstrap.alpine_keys.iterdir() if path.is_file())
    )
    inputs = (
        *target_runtime_artifacts(),
        binary,
        musl.install / "usr/lib/libc.so",
        *(artifact.source for artifact in userland),
        stress_tools,
        openssl.binary,
        bootstrap.apk_static,
        bootstrap.ca_certificates_bundle,
        bootstrap.private_key,
        bootstrap.public_key,
        *alpine_keys,
        ROOT / "user/base/passwd",
        ROOT / "user/base/shadow",
        ROOT / "user/base/group",
        ROOT / "user/base/inittab",
        ROOT / "user/base/profile",
        ROOT / "user/base/localtime",
        ROOT / "user/diagnostics/liteos-stress.c",
        boot_scene(),
        ROOT / "user/base/network-service",
        ROOT / "user/base/shutdown",
        ROOT / "user/base/udhcpc.script",
        ROOT / "user/base/interfaces",
        ROOT / "create_fs.py",
        Path(__file__).resolve(),
        ROOT / "scripts/apk_cache.py",
        ROOT / "scripts/apk_package.py",
        ROOT / "scripts/apk_rootfs.py",
        ROOT / "scripts/ext4_image.py",
        ROOT / "scripts/qemu_gate.py",
        find_mke2fs(),
        find_debugfs(),
        Path(host_openssl),
    )
    # rootfs 是纯构建产物，缓存身份只取决于输入 bytes 与 recipe。APK 数据库虽然要在
    # target guest 中生成，但 host QEMU 可执行文件的版本不改变最终镜像语义，不能进入缓存键。
    payload = {
        "kind": "busybox-rootfs-image",
        "recipe_version": ROOTFS_RECIPE_VERSION,
        "arch": TARGET.arch,
        "inputs": {str(path): sha256(path) for path in inputs},
    }
    identity = fingerprint(payload)
    cache = WORK / "rootfs-images" / identity
    with cache_lock(WORK / ".rootfs.lock"):
        if manifest_matches(cache, payload, ("fs.img",)):
            print(f"BusyBox rootfs cache hit: {identity[:12]}")
        else:
            temporary = temporary_directory(WORK / "rootfs-images", "rootfs")
            try:
                create_image(binary, musl, openssl, temporary / "fs.img")
                write_manifest(temporary, payload)
                publish_directory(temporary, cache)
            finally:
                shutil.rmtree(temporary, ignore_errors=True)

        # OWNER/PROOF: 同一 rootfs 锁覆盖发布、读取与淘汰，consumer 只拿独立副本。
        # FAILURE: 若复制在锁外，另一个 builder/清理进程可在 copyfile 之前删除源镜像。
        output.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix=".liteos-rootfs-", dir=output.parent) as directory:
            temporary = Path(directory) / "fs.img"
            shutil.copyfile(cache / "fs.img", temporary)
            temporary.replace(output)
        prune_rootfs_images(WORK, current=cache)
    return output


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
