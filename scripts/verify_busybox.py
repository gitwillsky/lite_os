#!/usr/bin/env python3
"""构建固定上游 BusyBox 动态 PIE，并校验唯一受控配置与 ELF 边界。"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

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
    runtime_gate_hit,
    runtime_gate_payload,
    sha256,
    temporary_directory,
    write_manifest,
)
from qemu_gate import boot, power_cut
from verify_musl import (
    MuslCachePaths,
    cached_musl_paths,
    compiler_identity,
    find_compiler,
    run,
)

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target" / "busybox-runtime"
CONFIG_FRAGMENT = ROOT / "user" / "busybox.config"
BUSYBOX_VERSION = "1.37.0"
BUSYBOX_URL = f"https://busybox.net/downloads/busybox-{BUSYBOX_VERSION}.tar.bz2"
BUSYBOX_SHA256 = "3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4"
SOURCE_RECIPE_VERSION = 1
BINARY_RECIPE_VERSION = 5
FORBIDDEN_BOOT_MARKERS = (
    "Invalid argument",
    "init: can't log to /dev/tty5",
    "unsupported syscall_id:",
)
BUSYBOX_LINKS = (
    "arch",
    "ash",
    "awk",
    "basename",
    "busybox",
    "cat",
    "cp",
    "cut",
    "date",
    "dd",
    "df",
    "dirname",
    "echo",
    "expr",
    "false",
    "find",
    "free",
    "grep",
    "gunzip",
    "gzip",
    "head",
    "id",
    "ifconfig",
    "chmod",
    "chown",
    "kill",
    "ln",
    "ls",
    "mkdir",
    "mv",
    "nc",
    "netstat",
    "printf",
    "ps",
    "pwd",
    "rm",
    "rmdir",
    "route",
    "sed",
    "seq",
    "setsid",
    "sha256sum",
    "sh",
    "sleep",
    "sort",
    "stty",
    "sync",
    "tail",
    "tee",
    "touch",
    "top",
    "tr",
    "true",
    "uniq",
    "uname",
    "uptime",
    "udhcpc",
    "wc",
    "wget",
    "zcat",
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


def source_payload() -> dict[str, object]:
    return {
        "kind": "busybox-source",
        "recipe_version": SOURCE_RECIPE_VERSION,
        "version": BUSYBOX_VERSION,
        "archive_sha256": BUSYBOX_SHA256,
        "strip_components": 1,
    }


def source_cache_path() -> Path:
    return WORK / "sources" / fingerprint(source_payload())


def obtain_source() -> Path:
    """获取并缓存固定官方源码；完整目录只在校验和解压成功后发布。"""
    archive = WORK / f"busybox-{BUSYBOX_VERSION}.tar.bz2"
    if not archive.is_file() or sha256(archive) != BUSYBOX_SHA256:
        archive.unlink(missing_ok=True)
        temporary = archive.with_suffix(".download")
        temporary.unlink(missing_ok=True)
        print(f"downloading BusyBox {BUSYBOX_VERSION}")
        try:
            urllib.request.urlretrieve(BUSYBOX_URL, temporary)
        except Exception as error:
            temporary.unlink(missing_ok=True)
            raise RuntimeError(f"failed to download {BUSYBOX_URL}: {error}") from error
        if sha256(temporary) != BUSYBOX_SHA256:
            temporary.unlink(missing_ok=True)
            raise RuntimeError("BusyBox release tarball SHA-256 mismatch")
        temporary.replace(archive)

    payload = source_payload()
    source = source_cache_path()
    if manifest_matches(source, payload, ("Makefile", "scripts/kconfig/mconf.c")):
        return source

    temporary = temporary_directory(WORK / "sources", "source")
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
    return {
        "kind": "busybox-dynamic-pie",
        "recipe_version": BINARY_RECIPE_VERSION,
        "source_fingerprint": fingerprint(source_payload()),
        "config_sha256": sha256(CONFIG_FRAGMENT),
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "compiler": compiler_identity(compiler),
        "architecture": "riscv",
        "drivers": {
            "compiler_sha256": sha256(ROOT / "scripts/musl_clang.py"),
            "linker_sha256": sha256(ROOT / "scripts/musl_ld.py"),
        },
        "strip": {
            "path": "/opt/homebrew/opt/llvm/bin/llvm-strip",
            "sha256": sha256(Path("/opt/homebrew/opt/llvm/bin/llvm-strip")),
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
            "LITEOS_MUSL_LIBGCC": str(musl.libgcc),
            "LITEOS_MUSL_SYSROOT": str(musl.install),
        })
        run(
            [
                *make_command(jobs_override),
                "-C",
                str(source),
                f"O={build}",
                "ARCH=riscv",
                f"CC={sys.executable} {ROOT / 'scripts/musl_clang.py'}",
                f"LD={sys.executable} {ROOT / 'scripts/musl_ld.py'}",
                f"AR={musl.archiver}",
                "STRIP=/opt/homebrew/opt/llvm/bin/llvm-strip",
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


def verify_elf(binary: Path, compiler: Path) -> None:
    """要求动态 RISC-V PIE、标准 musl interpreter、RELRO、W^X 与 NX stack。"""
    prefix = str(compiler)[: -len("gcc")]
    readelf = Path(f"{prefix}readelf")
    if not readelf.is_file():
        candidate = shutil.which("llvm-readelf") or "/opt/homebrew/opt/llvm/bin/llvm-readelf"
        readelf = Path(candidate)
    if not readelf.is_file():
        raise RuntimeError("RISC-V readelf or llvm-readelf is required")
    output = run(
        [str(readelf), "--file-header", "--program-headers", "--dynamic", "--wide", str(binary)], ROOT
    )
    for marker in ("ELF64", "RISC-V", "DYN ("):
        if marker not in output:
            raise RuntimeError(f"BusyBox ELF lacks {marker!r}")
    headers = [line.split() for line in output.splitlines()]
    if output.count("Requesting program interpreter:") != 1 or "/lib/ld-musl-riscv64.so.1" not in output:
        raise RuntimeError("BusyBox must use the standard RISC-V musl interpreter")
    for marker in ("DYNAMIC", "GNU_RELRO", "Shared library: [libc.so]", "NOW PIE"):
        if marker not in output:
            raise RuntimeError(f"BusyBox dynamic ELF lacks {marker!r}")
    if "TEXTREL" in output:
        raise RuntimeError("BusyBox dynamic ELF contains text relocations")
    loads = [columns for columns in headers if columns and columns[0] == "LOAD"]
    if not loads or not any(int(columns[1], 16) == 0 for columns in loads):
        raise RuntimeError("BusyBox PIE PHDR table is not covered by an offset-zero LOAD")
    for columns in headers:
        if len(columns) < 8 or columns[0] not in {"LOAD", "GNU_STACK"}:
            continue
        flags = "".join(columns[6:-1])
        if columns[0] == "LOAD" and "W" in flags and "E" in flags:
            raise RuntimeError("BusyBox contains a writable executable LOAD")
        if columns[0] == "GNU_STACK" and "E" in flags:
            raise RuntimeError("BusyBox requests an executable stack")


def find_debugfs() -> Path:
    candidates = (
        shutil.which("debugfs"),
        "/opt/homebrew/opt/e2fsprogs/sbin/debugfs",
        "/usr/local/opt/e2fsprogs/sbin/debugfs",
        "/usr/sbin/debugfs",
    )
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return Path(candidate)
    raise RuntimeError("debugfs from e2fsprogs is required")


def build_dynamic_probe(musl: MuslCachePaths) -> tuple[Path, Path]:
    payload = {
        "kind": "dynamic-loader-probe",
        "recipe_version": 1,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "driver_sha256": sha256(ROOT / "scripts/musl_clang.py"),
        "main_sha256": sha256(ROOT / "user/dynamic-smoke.c"),
        "library_sha256": sha256(ROOT / "user/dynamic-smoke-lib.c"),
    }
    entry = WORK / "dynamic-probes" / fingerprint(payload)
    if manifest_matches(entry, payload, ("dynamic-smoke", "libliteos-smoke.so")):
        return entry / "dynamic-smoke", entry / "libliteos-smoke.so"
    generation = generation_directory(WORK / "dynamic-probe-generations", fingerprint(payload))
    env = build_environment()
    env.update({
        "LITEOS_MUSL_CLANG": str(musl.compiler),
        "LITEOS_MUSL_LLD": str(musl.linker),
        "LITEOS_MUSL_LIBGCC": str(musl.libgcc),
        "LITEOS_MUSL_SYSROOT": str(musl.install),
    })
    published = False
    try:
        run(
            [
                str(musl.compiler),
                "--target=riscv64-linux-musl",
                f"--ld-path={musl.linker}",
                "-nostdlib",
                "-shared",
                "-fPIC",
                "-Wl,-z,relro,-z,now,-z,noexecstack",
                str(ROOT / "user/dynamic-smoke-lib.c"),
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
                str(ROOT / "user/dynamic-smoke.c"),
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


def create_image(binary: Path, musl: MuslCachePaths, image: Path) -> Path:
    """构造 BusyBox、唯一 musl runtime、标准 loader symlink 与固定 inittab。"""
    run(
        [
            sys.executable,
            "create_fs.py",
            "create",
            "--file",
            str(image),
            "--init",
            str(binary),
        ],
        ROOT,
    )
    dynamic_probe, dynamic_library = build_dynamic_probe(musl)
    commands = [
        "mkdir /etc",
        "mkdir /etc/init.d",
        "mkdir /lib",
        "mkdir /run",
        "mkdir /usr",
        "mkdir /usr/lib",
        "mkdir /usr/share",
        "mkdir /usr/share/udhcpc",
        f"write {ROOT / 'user' / 'inittab'} /etc/inittab",
        f"write {ROOT / 'user' / 'network-service'} /etc/init.d/network-service",
        "set_inode_field /etc/init.d/network-service mode 0100755",
        f"write {ROOT / 'user' / 'udhcpc.script'} /usr/share/udhcpc/default.script",
        "set_inode_field /usr/share/udhcpc/default.script mode 0100755",
        f"write {musl.install / 'usr/lib/libc.so'} /usr/lib/libc.so",
        "set_inode_field /usr/lib/libc.so mode 0100755",
        f"write {dynamic_library} /usr/lib/libliteos-smoke.so",
        f"write {dynamic_probe} /bin/dynamic-smoke",
        "set_inode_field /bin/dynamic-smoke mode 0100755",
        "symlink /lib/ld-musl-riscv64.so.1 /usr/lib/libc.so",
    ]
    commands.extend(f"ln /bin/init /bin/{applet}" for applet in BUSYBOX_LINKS)
    commands.append(f"set_inode_field /bin/init links_count {len(BUSYBOX_LINKS) + 1}")
    script_path: Path | None = None
    executable_script_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile("w", delete=False) as executable_script:
            executable_script.write(
                "#!/bin/sh -e\n"
                "[ \"$0\" = /bin/liteos-script ]\n"
                "[ \"$1:$2\" = 'alpha beta:omega' ]\n"
                "echo LITEOS_SCRIPT_EXEC_$((6*7))\n"
            )
            executable_script_path = Path(executable_script.name)
        commands.extend(
            (
                f"write {executable_script_path} /bin/liteos-script",
                "set_inode_field /bin/liteos-script mode 0100755",
            )
        )
        with tempfile.NamedTemporaryFile("w", delete=False) as script:
            script.write("\n".join(commands) + "\n")
            script_path = Path(script.name)
        run([str(find_debugfs()), "-w", "-f", str(script_path), str(image)], ROOT)
    finally:
        if script_path is not None:
            script_path.unlink(missing_ok=True)
        if executable_script_path is not None:
            executable_script_path.unlink(missing_ok=True)
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
    if f"Links: {len(expected)}" not in metadata:
        raise RuntimeError("BusyBox inode link count does not match rootfs applets")
    executable_script = run(
        [str(find_debugfs()), "-R", "stat /bin/liteos-script", str(image)], ROOT
    )
    if "Type: regular" not in executable_script or "Mode:  0755" not in executable_script:
        raise RuntimeError("BusyBox rootfs lacks executable shebang script")
    loader = run([str(find_debugfs()), "-R", "stat /lib/ld-musl-riscv64.so.1", str(image)], ROOT)
    if "Type: symlink" not in loader or "Size: 16" not in loader:
        raise RuntimeError("BusyBox rootfs lacks the standard musl loader symlink")
    network_service = run(
        [str(find_debugfs()), "-R", "stat /etc/init.d/network-service", str(image)], ROOT
    )
    if "Type: regular" not in network_service or "Mode:  0755" not in network_service:
        raise RuntimeError("BusyBox rootfs lacks the supervised network service")
    return image


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--build-only",
        action="store_true",
        help="只构建并校验固定 BusyBox rootfs，不启动 QEMU",
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
    try:
        WORK.mkdir(parents=True, exist_ok=True)
        jobs_override = build_jobs_override()
        compiler = find_compiler()
        with cache_lock(WORK / ".build.lock"):
            source = obtain_source()
            binary = build_busybox(source, compiler, jobs_override, args.rebuild)
            verify_elf(binary, compiler)
            musl = cached_musl_paths(compiler)
        if args.build_only:
            image = create_image(binary, musl, args.image.resolve())
            print(f"BusyBox {BUSYBOX_VERSION} rootfs build passed: {image}")
            return 0
        dynamic_probe, dynamic_library = build_dynamic_probe(musl)
        stamp = ROOT / "target/verify-gates/busybox.json"
        payload = runtime_gate_payload(
            "busybox-runtime",
            4,
            (
                ROOT / "target/riscv64gc-unknown-none-elf/debug/kernel",
                ROOT / "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
                binary,
                musl.install / "usr/lib/libc.so",
                dynamic_probe,
                dynamic_library,
                ROOT / "user/inittab",
                ROOT / "user/network-service",
                ROOT / "user/udhcpc.script",
                ROOT / "create_fs.py",
                Path(__file__).resolve(),
                ROOT / "scripts/qemu_gate.py",
            ),
        )
        image = args.image.resolve()
        if runtime_gate_hit(stamp, payload, (image,)):
            print(f"BusyBox {BUSYBOX_VERSION} init+ash verification cache hit")
            return 0
        image = create_image(binary, musl, image)
        runtime_directory = tempfile.TemporaryDirectory(prefix="liteos-busybox-gate-")
        runtime_image = Path(runtime_directory.name) / "fs.img"
        shutil.copyfile(image, runtime_image)
        http_server, http_port = start_http_gate()
        boot(
            runtime_image,
            1,
            (
                "dynamic hart topology initialized: count=1, mask=0x1",
                "all DTB harts online: count=1, mask=0x1",
                "init started: BusyBox v1.37.0",
                "LITEOS_BUSYBOX_SHELL_42",
                "LITEOS_DHCP_51",
                "LITEOS_DHCP_RESPAWN_51",
                "LITEOS_DNS_51",
                "LITEOS_HTTP_51",
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
            ),
            interactions=(
                (
                    "Please press Enter to activate this console.",
                    b"\n",
                ),
                (
                    "Enter 'help' for a list of built-in commands.",
                    b"echo LITEOS_BUSYBOX_SHELL_$((6*7))\n",
                ),
                (
                    "LITEOS_BUSYBOX_SHELL_42",
                    b"while [ ! -s /etc/resolv.conf ]; do /bin/sleep 1; done; count=$(/bin/ps | /bin/grep '[u]dhcpc' | /bin/wc -l); /bin/ifconfig eth0 | /bin/grep -q 'inet addr:10.0.2.15' && /bin/route | /bin/grep -q '^default .*10.0.2.2' && [ \"$count\" -eq 1 ] && echo LITEOS_DHCP_$((7*7+2))\n",
                ),
                (
                    "LITEOS_DHCP_51",
                    b"old=$(/bin/cat /run/udhcpc.eth0.pid); /bin/kill \"$old\"; while new=$(/bin/cat /run/udhcpc.eth0.pid 2>/dev/null); [ -z \"$new\" ] || [ \"$new\" = \"$old\" ]; do /bin/sleep 1; done; /bin/sleep 2; count=$(/bin/ps | /bin/grep '[u]dhcpc' | /bin/wc -l); [ \"$new\" != \"$old\" ] && [ \"$count\" -eq 1 ] && [ -s /etc/resolv.conf ] && /bin/route | /bin/grep -q '^default .*10.0.2.2' && echo LITEOS_DHCP_RESPAWN_$((7*7+2))\n",
                ),
                (
                    "LITEOS_DHCP_RESPAWN_51",
                    b"/bin/dynamic-smoke resolve example.com\n",
                ),
                (
                    "LITEOS_DNS_51",
                    f"/bin/wget -q -T 10 -O /http.out http://10.0.2.2:{http_port}/user/udhcpc.script && /bin/grep -q '^# @description BusyBox udhcpc' /http.out && echo LITEOS_HTTP_$((7*7+2))\n".encode(),
                ),
                (
                    "LITEOS_HTTP_51",
                    b"/bin/ls /; echo LITEOS_LS_$((6*7))\n",
                ),
                (
                    "LITEOS_LS_42",
                    b"/bin/ls /dev; echo HIDDEN >/dev/null; echo LITEOS_NULL_$((6*7)); /bin/dd if=/dev/zero of=/zero bs=4 count=1 2>/dev/null; set -- $(/bin/wc -c /zero); echo LITEOS_ZERO_$1; echo LITEOS_TTYDEV_$((6*7)) >/dev/tty; echo LITEOS_CONSOLEDEV_$((6*7)) >/dev/console; cd /dev; set -- $(/bin/pwd); [ \"$1\" = /dev ] && echo LITEOS_DEVCWD_$((6*7)); cd /\n",
                ),
                (
                    "LITEOS_DEVCWD_42",
                    b"/bin/printf 'pear\\napple\\napple\\n' > /words; set -- $(/bin/sort /words | /bin/uniq | /bin/wc -l); [ \"$1\" = 2 ] && echo LITEOS_TEXT_$((6*7))\n",
                ),
                (
                    "LITEOS_TEXT_42",
                    b"/bin/printf 'a:1\\nb:2\\nc:3\\n' | /bin/tee /data >/dev/null; a=$(/bin/sed -n '2p' /data | /bin/cut -d: -f2 | /bin/tr 2 7); b=$(/bin/awk -F: '{s+=$2} END {print s}' /data); c=$(/bin/head -n1 /data); d=$(/bin/tail -n1 /data); [ \"$a:$b:$c:$d\" = '7:6:a:1:c:3' ] && echo LITEOS_FILTERS_$((6*7))\n",
                ),
                (
                    "LITEOS_FILTERS_42",
                    b"/bin/mkdir /tools; /bin/touch /tools/a; n=$(/bin/find /tools -name a | /bin/wc -l); base=$(/bin/basename /a/b); dir=$(/bin/dirname /a/b); [ \"$n:$base:$dir\" = '1:b:/a' ] && echo LITEOS_FIND_$((6*7))\n",
                ),
                (
                    "LITEOS_FIND_42",
                    b"e=$(/bin/expr 6 \\* 7); last=$(/bin/seq 41 42 | /bin/tail -n1); [ \"$e:$last\" = '42:42' ] && echo LITEOS_MATH_$((6*7))\n",
                ),
                (
                    "LITEOS_MATH_42",
                    b"/bin/sleep 0; echo LITEOS_TOOLS_$((6*7))\n",
                ),
                (
                    "LITEOS_TOOLS_42",
                    b"/bin/ps > /ps.out; proc_total=$(/bin/awk '/^MemTotal:/ {print $2}' /proc/meminfo); free_total=$(/bin/free | /bin/awk '/^Mem:/ {print $2}'); /bin/grep -q init /ps.out && /bin/grep -q sh /ps.out && [ \"$proc_total\" = \"$free_total\" ] && /bin/uptime | /bin/grep -q 'load average:' && echo LITEOS_OBSERVABILITY_$((6*7))\n",
                ),
                (
                    "LITEOS_OBSERVABILITY_42",
                    b"/bin/grep -q '^root / ext2 rw 0 0$' /proc/mounts && /bin/grep -q '^devfs /dev devfs ro 0 0$' /proc/mounts && /bin/grep -q '^proc /proc proc ro 0 0$' /proc/mounts && /bin/df -Pk > /df.before; set -- $(/bin/awk 'NR==2 {print $2, $4, $6}' /df.before); total=$1; before=$2; mounted=$3; /bin/dd if=/dev/zero of=/df-space bs=4096 count=2 2>/dev/null; after=$(/bin/df -Pk / | /bin/awk 'NR==2 {print $4}'); inodes=$(/bin/df -Pi / | /bin/awk 'NR==2 {print $2}'); /bin/rm -f /df-space; [ ! -e /df-space ] && [ \"$total\" -gt 0 ] && [ \"$before\" -gt \"$after\" ] && [ \"$inodes\" -gt 0 ] && [ \"$mounted\" = / ] && echo LITEOS_FILESYSTEM_CAPACITY_$((6*7))\n",
                ),
                (
                    "LITEOS_FILESYSTEM_CAPACITY_42",
                    b"/bin/rm -rf /links; /bin/mkdir /links; echo alpha >/links/source; /bin/ln /links/source /links/hard; /bin/ln -s source /links/soft; [ \"$(/bin/cat /links/hard)\" = alpha ] && [ \"$(/bin/cat /links/soft)\" = alpha ] && echo beta >/links/hard; /bin/rm /links/source; [ \"$(/bin/cat /links/hard)\" = beta ] && /bin/ls -l /links/soft | /bin/grep -q -- '-> source' && echo LITEOS_LINKS_$((6*7+1))\n",
                ),
                (
                    "LITEOS_LINKS_43",
                    b"/bin/rm -rf /race; /bin/mkdir /race; workers=''; i=0; while [ $i -lt 8 ]; do (/bin/mkdir /race/d$i; j=0; while [ $j -lt 4 ]; do echo payload >/race/d$i/source$j && /bin/cp /race/d$i/source$j /race/d$i/copy$j && /bin/ln /race/d$i/copy$j /race/d$i/hard$j && /bin/ln -s hard$j /race/d$i/sym$j && /bin/mv /race/d$i/copy$j /race/d$i/moved$j && [ \"$(/bin/cat /race/d$i/sym$j)\" = payload ] && /bin/rm /race/d$i/sym$j /race/d$i/hard$j /race/d$i/moved$j /race/d$i/source$j || exit 1; j=$((j+1)); done; /bin/rmdir /race/d$i; echo done >>/race/completed) & workers=\"$workers $!\"; i=$((i+1)); done; ok=1; for worker in $workers; do wait $worker || ok=0; done; count=$(/bin/wc -l </race/completed); [ \"$ok:$count\" = 1:8 ] && echo LITEOS_NAMESPACE_CONCURRENCY_$((6*7+1))\n",
                ),
                (
                    "LITEOS_NAMESPACE_CONCURRENCY_43",
                    b"[ \"$(/bin/id -u):$(/bin/id -g)\" = 0:0 ] && /bin/touch /busybox-credential && /bin/chmod 640 /busybox-credential && /bin/chown 1000:1000 /busybox-credential && set -- $(/bin/ls -ln /busybox-credential); [ \"$1:$3:$4\" = '-rw-r-----:1000:1000' ] && echo LITEOS_BUSYBOX_CREDENTIALS_$((6*7+2))\n",
                ),
                (
                    "LITEOS_BUSYBOX_CREDENTIALS_44",
                    b"[ \"$(/bin/uname -s)\" = LiteOS ] && [ \"$(/bin/uname -n)\" = liteos ] && [ \"$(/bin/uname -m)\" = riscv64 ] && [ \"$(/bin/uname -o)\" = LiteOS ] && [ \"$(/bin/arch)\" = riscv64 ] && echo LITEOS_SYSTEM_IDENTITY_$((6*7))\n",
                ),
                (
                    "LITEOS_SYSTEM_IDENTITY_42",
                    b"epoch=$(/bin/date -u +%s); year=$(/bin/date -u +%Y); [ \"$epoch\" -ge 1704067200 ] && [ \"$year\" -ge 2024 ] && echo LITEOS_WALLCLOCK_$((6*7))\n",
                ),
                (
                    "LITEOS_WALLCLOCK_42",
                    b"{ read _; read _ free_before _; } </proc/meminfo; i=0; while [ $i -lt 64 ] && /bin/true; do i=$((i+1)); done; { read _; read _ free_after _; } </proc/meminfo; loss=$((free_before-free_after)); [ \"$i\" -eq 64 ] && [ \"$loss\" -le 4096 ] && echo LITEOS_EXEC_RECLAIM_$((6*7))\n",
                ),
                (
                    "LITEOS_EXEC_RECLAIM_42",
                    b"/bin/top -bn1 | /bin/grep -q init && echo LITEOS_TOP_$((6*7))\n",
                ),
                (
                    "LITEOS_TOP_42",
                    b"/bin/ls -l /lib | /bin/grep -q 'ld-musl-riscv64.so.1 -> /usr/lib/libc.so' && echo LITEOS_READLINK_$((6*7))\n",
                ),
                (
                    "LITEOS_READLINK_42",
                    b"/bin/dynamic-smoke\n",
                ),
                (
                    "LITEOS_UNIX_EPOLL_46",
                    b"",
                ),
                (
                    "LITEOS_DLOPEN_42",
                    b"/bin/liteos-script 'alpha beta' omega\n",
                ),
                (
                    "LITEOS_SCRIPT_EXEC_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"/bin/rm -f /kill.pid; /bin/setsid /bin/sh -c 'echo $$ >/kill.pid; trap \"echo LITEOS_KILL_GROUP_$((6*7)); exit 0\" TERM; echo LITEOS_KILL_\"READY\"; while :; do /bin/sleep 1; done' &\n",
                ),
                (
                    "LITEOS_KILL_READY",
                    b"read kill_pid < /kill.pid; /bin/kill -0 $kill_pid && /bin/kill -TERM -$kill_pid\n",
                ),
                (
                    "LITEOS_KILL_GROUP_42",
                    b"/bin/echo payload > /plain; /bin/gzip -c /plain > /plain.gz; a=$(/bin/zcat /plain.gz); b=$(/bin/gunzip -c /plain.gz); h=$(/bin/sha256sum /plain | /bin/cut -d' ' -f1); [ \"$a:$b:$h\" = 'payload:payload:d4e4877bac978b7952f0d544fc52ebff5411d351d129f1f056fa43f11da9af2b' ] && echo LITEOS_ARCHIVE_$((6*7))\n",
                ),
                (
                    "LITEOS_ARCHIVE_42",
                    b"/bin/echo LITEOS_PIPE_$((6*7)) | /bin/grep PIPE; echo LITEOS_REDIR_$((6*7)) > /redir; /bin/cat /redir; (echo LITEOS_BG_$((6*7)) > /bg) & wait; /bin/cat /bg; echo LITEOS_PERSIST_$((6*7)) > /persist; sync; echo LITEOS_PERSIST_WRITTEN_$((6*7))\n",
                ),
                (
                    "LITEOS_PERSIST_WRITTEN_42",
                    b"echo LITEOS_JOB_START_$((6*7)); /bin/sh -c 'while :; do :; done'\n",
                ),
                (
                    "LITEOS_JOB_START_42",
                    b"\x1a",
                ),
                (
                    "Stopped",
                    b"jobs > /jobs; /bin/grep -q Stopped /jobs && echo LITEOS_JOBS_STOPPED_$((6*7))\n",
                ),
                (
                    "LITEOS_JOBS_STOPPED_42",
                    b"bg; jobs > /jobs; /bin/grep -q Running /jobs && echo LITEOS_BG_CONTINUED_$((6*7))\n",
                ),
                (
                    "LITEOS_BG_CONTINUED_42",
                    b"echo LITEOS_FG_START_$((6*7)); fg\n",
                ),
                (
                    "LITEOS_FG_START_42",
                    b"\x03",
                ),
                (
                    "/ # ",
                    b"echo LITEOS_FG_CTRL_C_$((6*7)); echo LITEOS_TTY_CTRL_C_$((6*7))\n",
                ),
                (
                    "LITEOS_TTY_CTRL_C_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"/bin/dd if=/dev/tty of=/bgread bs=16 count=1 2>/dev/null & echo LITEOS_BG_READ_LAUNCHED_$((6*7))\n",
                ),
                (
                    "LITEOS_BG_READ_LAUNCHED_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"/bin/sleep 1; jobs > /jobs; /bin/grep -q Stopped /jobs && echo LITEOS_BG_READ_STOPPED_$((6*7))\n",
                ),
                (
                    "LITEOS_BG_READ_STOPPED_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"fg\n",
                ),
                (
                    "/bin/dd if=/dev/tty",
                    b"ttyinput\n",
                ),
                (
                    "/ # ",
                    b"/bin/grep -q '^ttyinput$' /bgread && echo LITEOS_BG_READ_OK_$((6*7))\n",
                ),
                (
                    "LITEOS_BG_READ_OK_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"/bin/sh -c 'trap \"\" 21; exec /bin/dd if=/dev/tty of=/ignored-read bs=1 count=1 2>/dev/null' & wait; [ ! -s /ignored-read ] && echo LITEOS_BG_READ_IGNORED_EIO_$((6*7))\n",
                ),
                (
                    "LITEOS_BG_READ_IGNORED_EIO_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"/bin/stty tostop </dev/tty; /bin/echo LITEOS_BG_WRITE_OK_$((6*7)) >/dev/tty & echo LITEOS_BG_WRITE_LAUNCHED_$((6*7))\n",
                ),
                (
                    "LITEOS_BG_WRITE_LAUNCHED_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"/bin/sleep 1; jobs > /jobs; /bin/grep -q Stopped /jobs && echo LITEOS_BG_WRITE_STOPPED_$((6*7))\n",
                ),
                (
                    "LITEOS_BG_WRITE_STOPPED_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"fg\n",
                ),
                (
                    "LITEOS_BG_WRITE_OK_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"/bin/sh -c 'trap \"\" 22; echo LITEOS_BG_WRITE_IGNORED_$((6*7)) >/dev/tty' & wait; /bin/stty -tostop </dev/tty\n",
                ),
                (
                    "LITEOS_BG_WRITE_IGNORED_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"/bin/kill -KILL 1; /bin/sleep 1; /bin/kill -0 1 && echo LITEOS_INIT_SURVIVED_$((6*7))\n",
                ),
                (
                    "LITEOS_INIT_SURVIVED_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"/bin/rm -f /orphan-result; /bin/sh -c 'trap \"echo LITEOS_ORPHAN_HUP_42 >/orphan-result\" 1; /bin/kill -STOP $$; echo LITEOS_ORPHAN_CONT_42 >>/orphan-result' & echo LITEOS_ORPHAN_LAUNCHED_$((6*7))\n",
                ),
                (
                    "LITEOS_ORPHAN_LAUNCHED_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"/bin/sleep 1; jobs > /jobs; /bin/grep -q Stopped /jobs && echo LITEOS_ORPHAN_STOPPED_$((6*7))\n",
                ),
                (
                    "LITEOS_ORPHAN_STOPPED_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"exit\n",
                ),
                (
                    "You have stopped jobs.",
                    b"exit\n",
                ),
                (
                    "Please press Enter to activate this console.",
                    b"\n",
                ),
                (
                    "Enter 'help' for a list of built-in commands.",
                    b"while [ ! -s /orphan-result ]; do /bin/sleep 1; done; /bin/cat /orphan-result\n",
                ),
                (
                    "LITEOS_ORPHAN_CONT_42",
                    b"",
                ),
                (
                    "/ # ",
                    b"/bin/rm -f /session-hup; /bin/sh -c 'trap \"echo LITEOS_SESSION_HUP_42 >/session-hup; exit 0\" 1; /bin/kill -KILL $PPID; while :; do /bin/sleep 1; done'\n",
                ),
                (
                    "Please press Enter to activate this console.",
                    b"\n",
                ),
                (
                    "Enter 'help' for a list of built-in commands.",
                    b"while [ ! -s /session-hup ]; do /bin/sleep 1; done; /bin/cat /session-hup\n",
                ),
            ),
            forbidden_markers=FORBIDDEN_BOOT_MARKERS,
            persistent_writes=True,
            timeout_seconds=90,
        )
        boot(
            runtime_image,
            8,
            (
                "dynamic hart topology initialized: count=8, mask=0xff",
                "all DTB harts online: count=8, mask=0xff",
                "init started: BusyBox v1.37.0",
                "LITEOS_PERSIST_42",
                "LITEOS_SHARED_PERSIST_45",
                "LITEOS_CREDENTIAL_PERSIST_44",
                "LITEOS_COW_ISOLATION_42",
                "LITEOS_SCHED_8_HARTS_42",
                "LITEOS_STREAMING_EXEC_42",
            ),
            interactions=(
                ("Please press Enter to activate this console.", b"\n"),
                (
                    "Enter 'help' for a list of built-in commands.",
                    b"/bin/cat /persist\n",
                ),
                (
                    "LITEOS_PERSIST_42",
                    b"/bin/cat /shared-persist\n",
                ),
                (
                    "LITEOS_SHARED_PERSIST_45",
                    b"set -- $(/bin/ls -ln /busybox-credential); [ \"$1:$3:$4\" = '-rw-r-----:1000:1000' ] && echo LITEOS_CREDENTIAL_PERSIST_$((6*7+2))\n",
                ),
                (
                    "LITEOS_CREDENTIAL_PERSIST_44",
                    b"x=parent; (x=child; [ \"$x\" = child ]) & wait; [ \"$x\" = parent ] && echo LITEOS_COW_ISOLATION_$((6*7))\n",
                ),
                (
                    "LITEOS_COW_ISOLATION_42",
                    b"pids=''; i=0; while [ $i -lt 8 ]; do (while :; do :; done) & pids=\"$pids $!\"; i=$((i+1)); done; echo LITEOS_SCHED_\"STARTED\"\n",
                ),
                (
                    "LITEOS_SCHED_STARTED",
                    b"/bin/sleep 2; n=$(/bin/awk '$1 ~ /^cpu[0-9]+$/ && $2 > 0 {n++} END {print n+0}' /proc/stat); echo LITEOS_SCHED_CPUS_$n; [ \"$n\" -eq 8 ] && echo LITEOS_SCHED_8_HARTS_$((6*7))\n",
                ),
                (
                    "LITEOS_SCHED_8_HARTS_42",
                    b"n=$(for p in $pids; do /bin/awk '{print $1}' /proc/$p/stat; done | /bin/sort -n | /bin/wc -l); [ \"$n\" -eq 8 ] && echo LITEOS_STREAMING_EXEC_$((6*7))\n",
                ),
            ),
            forbidden_markers=FORBIDDEN_BOOT_MARKERS,
            persistent_writes=True,
        )
        crash_image = Path(runtime_directory.name) / "crash.img"
        shutil.copyfile(image, crash_image)
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
                ("Please press Enter to activate this console.", b"\n"),
                (
                    "Enter 'help' for a list of built-in commands.",
                    b"/bin/rm -rf /crash; /bin/mkdir /crash; echo recovered >/crash/state; sync; [ \"$(/bin/cat /crash/state)\" = recovered ] && echo LITEOS_JOURNAL_RECOVERY_$((6*7+1))\n",
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
