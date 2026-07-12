#!/usr/bin/env python3
"""构建固定上游 BusyBox 静态 ET_EXEC，并校验唯一受控配置与 ELF 边界。"""

from __future__ import annotations

import hashlib
import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

from qemu_gate import boot
from verify_musl import cached_musl_paths, find_compiler, run

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "target" / "busybox-static"
CONFIG_FRAGMENT = ROOT / "user" / "busybox.config"
BUSYBOX_VERSION = "1.37.0"
BUSYBOX_URL = f"https://busybox.net/downloads/busybox-{BUSYBOX_VERSION}.tar.bz2"
BUSYBOX_SHA256 = "3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4"
BUSYBOX_LINKS = (
    "ash",
    "busybox",
    "cat",
    "cp",
    "dd",
    "echo",
    "false",
    "grep",
    "ls",
    "mkdir",
    "mv",
    "printf",
    "pwd",
    "rm",
    "rmdir",
    "sh",
    "sync",
    "touch",
    "true",
    "wc",
)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def obtain_source() -> Path:
    """获取并校验官方 release tarball，不接受同版本的其他来源。"""
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

    source = WORK / "source"
    shutil.rmtree(source, ignore_errors=True)
    source.mkdir(parents=True)
    run(
        ["tar", "-xjf", str(archive), "--strip-components=1", "-C", str(source)],
        ROOT,
    )
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


def configure(source: Path, env: dict[str, str]) -> None:
    """从全关闭状态应用唯一 fragment，避免 BusyBox 默认 applet 隐式进入产物。"""
    run(["make", "allnoconfig"], source, env)
    config = source / ".config"
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
        ["make", "oldconfig"],
        cwd=source,
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


def build_busybox(source: Path, compiler: Path) -> Path:
    """使用上一 gate 产出的固定 musl sysroot 构建静态 BusyBox。"""
    musl = cached_musl_paths(compiler)
    env = os.environ.copy()
    env["LC_ALL"] = "C"
    for name in ("CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "LIBRARY_PATH"):
        env.pop(name, None)
    configure(source, env)

    specs = WORK / "musl-gcc.specs"
    result = subprocess.run(
        [
            "sh",
            str(musl.source / "tools" / "musl-gcc.specs.sh"),
            str(musl.install / "include"),
            str(musl.install / "lib"),
            "/lib/ld-musl-riscv64.so.1",
        ],
        cwd=ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"failed to generate musl GCC specs\n{result.stdout}")
    specs_text = result.stdout
    if run([str(compiler), "-print-file-name=crtbeginS.o"], ROOT).strip() == "crtbeginS.o":
        # bare-metal GCC 只提供等价的静态 crtbegin/crtend；缺少此适配会在最终链接时误报库探测失败。
        specs_text = specs_text.replace("crtbeginS.o%s", "crtbegin.o%s")
        specs_text = specs_text.replace("crtendS.o%s", "crtend.o%s")
        # 同一工具链默认追加 newlib 的 libgloss；musl 静态链接必须只有唯一 libc provider。
        specs_text = specs_text.replace(
            "%rename cpp_options old_cpp_options",
            "%rename cpp_options old_cpp_options\n%rename lib old_lib",
            1,
        )
        specs_text = specs_text.replace("\n*esp_link:", "\n*lib:\n-lc\n\n*esp_link:", 1)
    specs.write_text(specs_text)

    prefix = str(compiler)[: -len("gcc")]
    jobs = str(min(os.cpu_count() or 1, 8))
    run(
        [
            "make",
            f"-j{jobs}",
            "ARCH=riscv",
            f"CROSS_COMPILE={prefix}",
            f"CC={compiler} -specs={specs}",
        ],
        source,
        env,
    )
    binary = source / "busybox"
    if not binary.is_file():
        raise RuntimeError("BusyBox build did not produce busybox")
    return binary


def verify_elf(binary: Path, compiler: Path) -> None:
    """要求静态 RISC-V ET_EXEC、非 W+X LOAD 与不可执行用户栈。"""
    prefix = str(compiler)[: -len("gcc")]
    readelf = Path(f"{prefix}readelf")
    if not readelf.is_file():
        candidate = shutil.which("llvm-readelf") or "/opt/homebrew/opt/llvm/bin/llvm-readelf"
        readelf = Path(candidate)
    if not readelf.is_file():
        raise RuntimeError("RISC-V readelf or llvm-readelf is required")
    output = run(
        [str(readelf), "--file-header", "--program-headers", "--wide", str(binary)], ROOT
    )
    for marker in ("ELF64", "RISC-V", "EXEC"):
        if marker not in output:
            raise RuntimeError(f"BusyBox ELF lacks {marker!r}")
    headers = [line.split() for line in output.splitlines()]
    if any(columns and columns[0] in {"INTERP", "DYNAMIC"} for columns in headers):
        raise RuntimeError("BusyBox must remain a static ET_EXEC")
    loads = [columns for columns in headers if columns and columns[0] == "LOAD"]
    if not loads or not any(int(columns[1], 16) == 0 for columns in loads):
        raise RuntimeError("BusyBox PHDR table is not covered by an offset-zero LOAD")
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


def create_image(binary: Path, image: Path) -> Path:
    """构造单一 BusyBox inode、hardlink applets 与固定 inittab 的 ext2 rootfs。"""
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
    commands = [
        "mkdir /etc",
        f"write {ROOT / 'user' / 'inittab'} /etc/inittab",
    ]
    commands.extend(f"ln /bin/init /bin/{applet}" for applet in BUSYBOX_LINKS)
    commands.append(f"set_inode_field /bin/init links_count {len(BUSYBOX_LINKS) + 1}")
    script_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile("w", delete=False) as script:
            script.write("\n".join(commands) + "\n")
            script_path = Path(script.name)
        run([str(find_debugfs()), "-w", "-f", str(script_path), str(image)], ROOT)
    finally:
        if script_path is not None:
            script_path.unlink(missing_ok=True)
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
    args = parser.parse_args()
    try:
        WORK.mkdir(parents=True, exist_ok=True)
        compiler = find_compiler()
        source = obtain_source()
        binary = build_busybox(source, compiler)
        verify_elf(binary, compiler)
        image = create_image(binary, args.image.resolve())
        if args.build_only:
            print(f"BusyBox {BUSYBOX_VERSION} rootfs build passed: {image}")
            return 0
        boot(
            image,
            1,
            (
                "dynamic hart topology initialized: count=1, mask=0x1",
                "all DTB harts online: count=1, mask=0x1",
                "init started: BusyBox v1.37.0",
                "LITEOS_BUSYBOX_SHELL_42",
                "LITEOS_LS_42",
                "LITEOS_NULL_42",
                "LITEOS_ZERO_4",
                "LITEOS_TTYDEV_42",
                "LITEOS_CONSOLEDEV_42",
                "LITEOS_DEVCWD_42",
                "LITEOS_PIPE_42",
                "LITEOS_REDIR_42",
                "LITEOS_BG_42",
                "LITEOS_PERSIST_WRITTEN_42",
                "LITEOS_TTY_CTRL_C_42",
            ),
            interactions=(
                (
                    "Please press Enter to activate this console.",
                    b"\necho LITEOS_BUSYBOX_SHELL_$((6*7))\n",
                ),
                (
                    "LITEOS_BUSYBOX_SHELL_42",
                    b"/bin/ls /; echo LITEOS_LS_$((6*7))\n",
                ),
                (
                    "LITEOS_LS_42",
                    b"/bin/ls /dev; echo HIDDEN >/dev/null; echo LITEOS_NULL_$((6*7)); /bin/dd if=/dev/zero of=/zero bs=4 count=1 2>/dev/null; set -- $(/bin/wc -c /zero); echo LITEOS_ZERO_$1; echo LITEOS_TTYDEV_$((6*7)) >/dev/tty; echo LITEOS_CONSOLEDEV_$((6*7)) >/dev/console; cd /dev; set -- $(/bin/pwd); [ \"$1\" = /dev ] && echo LITEOS_DEVCWD_$((6*7)); cd /\n",
                ),
                (
                    "LITEOS_DEVCWD_42",
                    b"/bin/echo LITEOS_PIPE_$((6*7)) | /bin/grep PIPE; echo LITEOS_REDIR_$((6*7)) > /redir; /bin/cat /redir; (echo LITEOS_BG_$((6*7)) > /bg) & wait; /bin/cat /bg; echo LITEOS_PERSIST_$((6*7)) > /persist; sync; echo LITEOS_PERSIST_WRITTEN_$((6*7))\n",
                ),
                (
                    "LITEOS_PERSIST_WRITTEN_42",
                    b"echo LITEOS_TTY_LOOP_$((6*7)); while :; do :; done\n",
                ),
                (
                    "LITEOS_TTY_LOOP_42",
                    b"\x03echo LITEOS_TTY_CTRL_C_$((6*7))\n",
                ),
            ),
            forbidden_markers=("Invalid argument",)
            + tuple(
                f"unsupported syscall_id: {number}"
                for number in (29, 59, 65, 73, 81, 133, 137, 142, 154, 155, 156, 157, 174, 175, 176, 177)
            ),
        )
        boot(
            image,
            8,
            (
                "dynamic hart topology initialized: count=8, mask=0xff",
                "all DTB harts online: count=8, mask=0xff",
                "init started: BusyBox v1.37.0",
                "LITEOS_PERSIST_42",
            ),
            interactions=(("Please press Enter to activate this console.", b"\n/bin/cat /persist\n"),),
            forbidden_markers=tuple(
                f"unsupported syscall_id: {number}"
                for number in (29, 59, 65, 73, 81, 133, 137, 142, 154, 155, 156, 157, 174, 175, 176, 177)
            ),
        )
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"BusyBox verification failed: {error}", file=sys.stderr)
        return 1
    print(f"BusyBox {BUSYBOX_VERSION} init+ash verification passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
