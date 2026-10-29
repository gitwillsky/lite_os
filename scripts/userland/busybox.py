"""BusyBox 固定源码、配置、ELF 与二进制缓存发布。"""
from __future__ import annotations

import shutil
import subprocess
import sys
import urllib.request
from build_cache import (
    build_environment,
    fingerprint,
    generation_directory,
    make_command,
    manifest_matches,
    publish_directory,
    publish_generation,
    sha256,
    temporary_directory,
    write_manifest,
)
from build_target import target_from_environment
from local_inputs import INPUTS
from pathlib import Path
from toolchain.musl import MuslCachePaths, cached_musl_paths, compiler_identity, find_stripper, run

ROOT = Path(__file__).resolve().parents[2]
TARGET = target_from_environment()
WORK = ROOT / "target" / "busybox-runtime" / TARGET.arch

SOURCE = INPUTS / "busybox" / TARGET.arch

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

if TARGET.arch == "aarch64":
    BUSYBOX_ARCH = "arm64"
    BUSYBOX_TARGET_CFLAGS = "-march=armv8-a"
    ELF_MACHINE = "AArch64"
else:
    BUSYBOX_ARCH = "riscv"
    BUSYBOX_TARGET_CFLAGS = "-march=rv64gc -mabi=lp64d"
    ELF_MACHINE = "RISC-V"

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

