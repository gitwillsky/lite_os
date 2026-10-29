"""musl consumer 的 ELF 与 QEMU runtime 门禁。"""
from __future__ import annotations

import argparse
import shutil
import struct
import subprocess
import sys
import tempfile
from build_cache import (
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
from pathlib import Path
from qemu_gate import boot, cpu_topology_markers
from toolchain.musl import (
    ELF_MACHINE,
    MUSL_VERSION,
    SMOKE_LINK_ARGUMENTS,
    build_musl,
    compiler_identity,
    find_compiler,
    obtain_compiler_runtime,
    obtain_linux_source,
    obtain_source,
    run,
)

ROOT = Path(__file__).resolve().parent.parent
TARGET = target_from_environment()
WORK = ROOT / "target" / "musl-runtime" / TARGET.arch

SMOKE_RECIPE_VERSION = 22


def smoke_payload(install: Path, compiler: Path, sysroot_fingerprint: str) -> dict[str, object]:
    compiler_runtime = obtain_compiler_runtime(compiler)
    return {
        "kind": "musl-smoke",
        "recipe_version": SMOKE_RECIPE_VERSION,
        "arch": TARGET.arch,
        "sysroot_fingerprint": sysroot_fingerprint,
        "compiler": compiler_identity(compiler),
        "source_sha256": sha256(ROOT / "scripts/fixtures/musl/musl-smoke.c"),
        "shared_sync_sha256": sha256(ROOT / "scripts/fixtures/musl/shared-sync.c"),
        "process_security_sha256": sha256(ROOT / "scripts/fixtures/musl/process-security.c"),
        "fs_context_sha256": sha256(ROOT / "scripts/fixtures/musl/fs-context.c"),
        "file_table_sha256": sha256(ROOT / "scripts/fixtures/musl/file-table.c"),
        "thread_mm_sha256": sha256(ROOT / "scripts/fixtures/musl/thread-mm.c"),
        "capabilities_sha256": sha256(ROOT / "scripts/fixtures/musl/capabilities.c"),
        "anonymous_files_sha256": sha256(ROOT / "scripts/fixtures/musl/anonymous-files.c"),
        "fs_credentials_sha256": sha256(ROOT / "scripts/fixtures/musl/fs-credentials.c"),
        "fs_root_sha256": sha256(ROOT / "scripts/fixtures/musl/fs-root.c"),
        "fs_root_dynamic_sha256": sha256(ROOT / "scripts/fixtures/musl/fs-root-dynamic.c"),
        "sighand_sha256": sha256(ROOT / "scripts/fixtures/musl/sighand.c"),
        "namespace_user_sha256": sha256(ROOT / "scripts/fixtures/musl/namespace-user.c"),
        "pidfd_sha256": sha256(ROOT / "scripts/fixtures/musl/pidfd.c"),
        "udp_sha256": sha256(ROOT / "scripts/fixtures/musl/udp.c"),
        "network_namespace_sha256": sha256(ROOT / "scripts/fixtures/musl/network-namespace.c"),
        "netlink_sha256": sha256(ROOT / "scripts/fixtures/musl/netlink.c"),
        "ipv4_routing_sha256": sha256(ROOT / "scripts/fixtures/musl/ipv4-routing.c"),
        "thread_exec_sha256": sha256(ROOT / "scripts/fixtures/musl/thread-exec.c"),
        "seccomp_sha256": sha256(ROOT / "scripts/fixtures/musl/seccomp.c"),
        "seccomp_notification_sha256": sha256(ROOT / "scripts/fixtures/musl/seccomp_notification.c"),
        "ptrace_sha256": sha256(ROOT / "scripts/fixtures/musl/ptrace.c"),
        "signal_frame_sha256": sha256(ROOT / "scripts/fixtures/musl/signal-frame.c"),
        "swap_sha256": sha256(ROOT / "scripts/fixtures/musl/swap.c"),
        "address_space_sha256": sha256(ROOT / "scripts/fixtures/musl/address-space.c"),
        "netlink_route_sha256": sha256(ROOT / "scripts/fixtures/musl/netlink-route.c"),
        "copy_file_range_sha256": sha256(ROOT / "scripts/fixtures/musl/copy-file-range.c"),
        "io_uring_sha256": sha256(ROOT / "scripts/fixtures/musl/io-uring.c"),
        "statx_sha256": sha256(ROOT / "scripts/fixtures/musl/statx.c"),
        "mremap_sha256": sha256(ROOT / "scripts/fixtures/musl/mremap.c"),
        "resource_usage_sha256": sha256(ROOT / "scripts/fixtures/musl/resource-usage.c"),
        "memory_pressure_sha256": sha256(ROOT / "scripts/fixtures/musl/memory-pressure.c"),
        "link_arguments": list(SMOKE_LINK_ARGUMENTS),
        "compiler_runtime": {
            "path": str(compiler_runtime),
            "sha256": sha256(compiler_runtime),
        },
        "install": str(install),
    }


def link_smoke(
    install: Path,
    compiler: Path,
    sysroot_fingerprint: str,
    rebuild: bool = False,
) -> Path:
    """按 consumer/sysroot/link recipe fingerprint 链接或复用静态 ET_EXEC。"""
    payload = smoke_payload(install, compiler, sysroot_fingerprint)
    smoke_fingerprint = fingerprint(payload)
    directory = WORK / "smoke" / smoke_fingerprint
    output = directory / "musl-smoke"
    if not rebuild and manifest_matches(directory, payload, ("musl-smoke", "fs-root-dynamic", "libc.so")):
        print(f"musl smoke cache hit: {smoke_fingerprint[:12]}")
        return output

    generation = generation_directory(WORK / "smoke-generations", smoke_fingerprint)
    generation_output = generation / "musl-smoke"
    compiler_runtime = obtain_compiler_runtime(compiler)
    compiler_arguments = [
        str(compiler),
        f"--target={TARGET.linux_triple}",
        f"--ld-path={install / 'toolchain/ld.lld'}",
    ]
    published = False
    try:
        run(
            [
                *compiler_arguments,
                *SMOKE_LINK_ARGUMENTS,
                f"-I{install / 'usr/include'}",
                "-o",
                str(generation_output),
                str(install / "usr/lib" / "crt1.o"),
                str(install / "usr/lib" / "crti.o"),
                str(ROOT / "scripts/fixtures/musl/musl-smoke.c"),
                str(ROOT / "scripts/fixtures/musl/shared-sync.c"),
                str(ROOT / "scripts/fixtures/musl/process-security.c"),
                str(ROOT / "scripts/fixtures/musl/fs-context.c"),
                str(ROOT / "scripts/fixtures/musl/file-table.c"),
                str(ROOT / "scripts/fixtures/musl/thread-mm.c"),
                str(ROOT / "scripts/fixtures/musl/capabilities.c"),
                str(ROOT / "scripts/fixtures/musl/sighand.c"),
                str(ROOT / "scripts/fixtures/musl/fs-root.c"),
                str(ROOT / "scripts/fixtures/musl/fs-credentials.c"),
                str(ROOT / "scripts/fixtures/musl/anonymous-files.c"),
                str(ROOT / "scripts/fixtures/musl/thread-exec.c"),
                str(ROOT / "scripts/fixtures/musl/namespace-user.c"),
                str(ROOT / "scripts/fixtures/musl/pidfd.c"),
                str(ROOT / "scripts/fixtures/musl/udp.c"),
                str(ROOT / "scripts/fixtures/musl/network-namespace.c"),
                str(ROOT / "scripts/fixtures/musl/netlink.c"),
                str(ROOT / "scripts/fixtures/musl/ipv4-routing.c"),
                str(ROOT / "scripts/fixtures/musl/seccomp.c"),
                str(ROOT / "scripts/fixtures/musl/seccomp_notification.c"),
                str(ROOT / "scripts/fixtures/musl/signal-frame.c"),
                str(ROOT / "scripts/fixtures/musl/ptrace.c"),
                str(ROOT / "scripts/fixtures/musl/address-space.c"),
                str(ROOT / "scripts/fixtures/musl/memory-pressure.c"),
                str(ROOT / "scripts/fixtures/musl/resource-usage.c"),
                str(ROOT / "scripts/fixtures/musl/mremap.c"),
                str(ROOT / "scripts/fixtures/musl/statx.c"),
                str(ROOT / "scripts/fixtures/musl/copy-file-range.c"),
                str(ROOT / "scripts/fixtures/musl/io-uring.c"),
                str(ROOT / "scripts/fixtures/musl/netlink-route.c"),
                str(ROOT / "scripts/fixtures/musl/swap.c"),
                f"-L{install / 'usr/lib'}",
                "-Wl,--start-group",
                str(install / "usr/lib/libc.a"),
                str(compiler_runtime),
                "-Wl,--end-group",
                str(install / "usr/lib" / "crtn.o"),
            ],
            ROOT,
        )
        dynamic_arguments = [argument for argument in SMOKE_LINK_ARGUMENTS if argument != "-static"]
        run([
            *compiler_arguments, *dynamic_arguments, "-no-pie",
            f"-I{install / 'usr/include'}", f"-Wl,-dynamic-linker,{TARGET.musl_loader}",
            "-o", str(generation / "fs-root-dynamic"),
            str(install / "usr/lib/crt1.o"), str(install / "usr/lib/crti.o"),
            str(ROOT / "scripts/fixtures/musl/fs-root-dynamic.c"),
            f"-L{install / 'usr/lib'}", "-Wl,-Bdynamic", "-lc", str(install / "usr/lib/crtn.o"),
        ], ROOT)
        shutil.copy2(install / "usr/lib/libc.so", generation / "libc.so")
        write_manifest(generation, payload)
        publish_generation(generation, directory)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    print(f"musl smoke cache populated: {smoke_fingerprint[:12]}")
    return output


def verify_elf(binary: Path, compiler: Path) -> None:
    """校验静态主程序，以及同一 generation 内真实 PT_INTERP helper 的依赖。"""
    prefix = str(compiler)[: -len("gcc")]
    readelf = Path(f"{prefix}readelf")
    if not readelf.is_file():
        candidate = shutil.which("llvm-readelf") or "/opt/homebrew/opt/llvm/bin/llvm-readelf"
        readelf = Path(candidate)
    if not readelf.is_file():
        raise RuntimeError(f"{TARGET.arch} readelf or llvm-readelf is required")
    output = run(
        [str(readelf), "--file-header", "--program-headers", "--wide", str(binary)], ROOT
    )
    for marker in ("ELF64", ELF_MACHINE, "EXEC"):
        if marker not in output:
            raise RuntimeError(f"musl smoke ELF lacks {marker!r}")
    headers = [line.split() for line in output.splitlines()]
    if any(columns and columns[0] in {"INTERP", "DYNAMIC"} for columns in headers):
        raise RuntimeError("musl smoke must remain a static ET_EXEC")
    loads = [columns for columns in headers if columns and columns[0] == "LOAD"]
    if not loads or not any(int(columns[1], 16) == 0 for columns in loads):
        raise RuntimeError("musl smoke PHDR table is not covered by an offset-zero LOAD")
    for columns in headers:
        if len(columns) < 8 or columns[0] not in {"LOAD", "GNU_STACK"}:
            continue
        flags = "".join(columns[6:-1])
        if columns[0] == "LOAD" and "W" in flags and "E" in flags:
            raise RuntimeError("musl smoke contains a writable executable LOAD")
        if columns[0] == "GNU_STACK" and "E" in flags:
            raise RuntimeError("musl smoke requests an executable stack")

    dynamic = run(
        [str(readelf), "--file-header", "--program-headers", "--dynamic", "--wide", str(binary.parent / "fs-root-dynamic")], ROOT
    )
    for marker in ("ELF64", ELF_MACHINE, "EXEC", f"[Requesting program interpreter: {TARGET.musl_loader}]"):
        if marker not in dynamic:
            raise RuntimeError(f"musl dynamic probe ELF lacks {marker!r}")
    needed = [line.split("[", 1)[1].split("]", 1)[0] for line in dynamic.splitlines() if "(NEEDED)" in line and "[" in line]
    if needed != ["libc.so"]:
        raise RuntimeError(f"musl dynamic probe must require bundled libc.so, got {needed!r}")


def create_image(binary: Path) -> Path:
    image = WORK / "fs.img"
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
    from ext4_image import run_debugfs
    # Consumer generation owns the static main, real dynamic probe and matching libc.
    # Installing all three is required for the rooted PT_INTERP regression in full smoke.
    for directory in ("/lib", "/usr", "/usr/lib"):
        run_debugfs(image, f"mkdir {directory}", writable=True)
    for source, destination in ((binary.parent / "fs-root-dynamic", "/bin/fs-root-dynamic"),
                                (binary.parent / "libc.so", "/usr/lib/libc.so")):
        run_debugfs(image, f"write {source} {destination}", writable=True)
        run_debugfs(image, f"set_inode_field {destination} mode 0100755", writable=True)
    run_debugfs(image, f"symlink {TARGET.musl_loader} /usr/lib/libc.so", writable=True)
    # io_uring 的冷读用例需要“本次启动里从未被读过”的 ext4 文件，才能走 page cache 未命中的异步填页；
    # 内容公式与 io-uring.c 的 cold_byte 一致，其中一整页为零（hole）。
    for name, size, hole in (("a", 5 * 4096 + 1234, 2), ("b", 8 * 4096, None),
                             ("c", 3 * 4096, None), ("d", 2 * 4096, None),
                             ("e", 3 * 4096, None)):
        data = bytearray(((i * 131) + (i >> 12) * 17 + 7) & 0xFF for i in range(size))
        if hole is not None:
            data[hole * 4096:(hole + 1) * 4096] = bytes(4096)
        source = WORK / f"uring-cold-{name}.bin"
        source.write_bytes(data)
        run_debugfs(image, f"write {source} /usr/lib/uring-cold-{name}.bin", writable=True)
    return image


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--build-only",
        action="store_true",
        help="只构建并校验固定 musl consumer，不创建镜像或启动 QEMU",
    )
    parser.add_argument(
        "--rebuild",
        action="store_true",
        help="忽略当前 fingerprint 的 sysroot/smoke 命中并重新构建",
    )
    args = parser.parse_args()
    try:
        jobs_override = build_jobs_override()
        compiler = find_compiler()
        with cache_lock(WORK / ".build.lock"):
            source = obtain_source()
            linux_source = obtain_linux_source()
            install, sysroot_fingerprint = build_musl(
                source, linux_source, compiler, jobs_override, args.rebuild
            )
            binary = link_smoke(install, compiler, sysroot_fingerprint, args.rebuild)
            verify_elf(binary, compiler)
        if args.build_only:
            print(f"musl {MUSL_VERSION} runtime build passed")
            return 0
        stamp = ROOT / f"target/verify-gates/musl-{TARGET.arch}.json"
        runtime_inputs = [
            ROOT / f"target/{TARGET.kernel_triple}/release/kernel",
            binary,
            binary.parent / "fs-root-dynamic",
            binary.parent / "libc.so",
            ROOT / "create_fs.py",
            Path(__file__).resolve(),
            ROOT / "scripts/toolchain/musl.py",
            ROOT / "scripts/qemu_gate.py",
        ]
        if TARGET.requires_bootloader:
            runtime_inputs.insert(
                1,
                ROOT
                / f"bootloader/target/{TARGET.kernel_triple}/release/bootloader",
            )
        payload = runtime_gate_payload(
            "musl-runtime",
            5,
            tuple(runtime_inputs),
        )
        image = WORK / "fs.img"
        if runtime_gate_hit(stamp, payload, (image,)):
            print(f"musl {MUSL_VERSION} pthread signal swap verification cache hit")
            return 0
        image = create_image(binary)
        for cpu_count in (1, 2):
            boot(
                image,
                cpu_count,
                (
                    *cpu_topology_markers(cpu_count),
                    "LiteOS musl mmap placement ok",
                    "LiteOS musl RAW255 header ownership and send only delivery ok",
                    "LiteOS musl ASID ok",
                    "LiteOS musl raw thread credentials and NPROC ok",
                    "LiteOS musl fs context sharing and unshare ok",
                    "LiteOS musl file table sharing exec and POSIX close race ok",
                    "LiteOS musl thread mm lifetime nonleader exec identity and proc views ok",
                    "LiteOS musl capabilities thread ownership UID exec and zombie query ok",
                    "LiteOS musl sighand sharing exec detach pending discard and one shot ok",
                    "LiteOS musl fs root chroot lookup exec and proc views ok",
                    "LiteOS musl filesystem IDs dumpability exec proc and parent death ok",
                    "LiteOS musl anonymous inode identity reopen metadata session and statfs ok",
                    "LiteOS musl libc thread credential synchronization ok",
                    "LiteOS musl credential leader retirement ok",
                    "LiteOS musl pthread signal ok",
                    "LiteOS musl consumer exit=0",
                ),
                forbidden_markers=("LiteOS musl consumer failed", "LiteOS musl ASID failed", "LiteOS musl mmap placement failed", " fail step="),
            )
        with tempfile.TemporaryDirectory(prefix="liteos-swap-") as directory:
            scratch = Path(directory) / "swap.img"
            # Linux v1 4KiB header，磁盘格式与坏页/边界裁决另由生产 parser host 回归覆盖。
            pages = 64 * 1024 * 1024 // 4096
            header = bytearray(4096)
            struct.pack_into("<III", header, 1024, 1, pages - 1, 0)
            header[4086:] = b"SWAPSPACE2"
            with scratch.open("wb") as disk:
                disk.write(header)
                disk.truncate(pages * 4096)
            for cpu_count in (1, 2):
                boot(
                    image, cpu_count,
                    (*cpu_topology_markers(cpu_count), "LiteOS musl swapoff EINTR retry ok", "LiteOS musl swap ok", "LiteOS musl consumer exit=0"),
                    timeout_seconds=120,
                    forbidden_markers=("LiteOS musl swap failed", "LiteOS musl consumer failed"),
                    memory="128M",
                    kernel_arguments="init=/bin/init -- swap-smoke",
                    scratch_disk=scratch,
                )
        publish_runtime_gate(stamp, payload)
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"musl verification failed: {error}", file=sys.stderr)
        return 1
    print(f"musl {MUSL_VERSION} pthread signal swap verification passed")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
