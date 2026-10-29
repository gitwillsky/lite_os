"""Rust std consumer 的构建与 QEMU runtime 门禁。"""
from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tempfile
from build_cache import (
    build_environment,
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
from ext4_image import find_debugfs
from pathlib import Path
from qemu_gate import ShellCommand, boot, cpu_topology_markers
from toolchain.unwind import build_libunwind, rust_source_root
from verify_busybox import start_http_gate
from userland.rust import verify_elf
from toolchain.musl import MuslCachePaths, cached_musl_paths, find_compiler, run

ROOT = Path(__file__).resolve().parent.parent
TARGET = target_from_environment()
WORK = ROOT / "target" / "rust-std-runtime" / TARGET.arch

CRATE = ROOT / "scripts" / "fixtures" / "rust-std"

if TARGET.arch == "aarch64":
    RUST_USER_TARGET = "aarch64-unknown-linux-musl"
else:
    RUST_USER_TARGET = "riscv64gc-unknown-linux-musl"

BASE_RUST_FLAGS = (
    "-C link-self-contained=no "
    "-C target-feature=-crt-static "
    "-C relocation-model=pic "
    "-C panic=abort "
    "-C link-arg=-Wl,--gc-sections,-z,relro,-z,now,-z,noexecstack "
    "-D warnings"
)

STD_MARKERS = (
    "LITEOS_RUST_STD_ALLOC_61",
    "LITEOS_RUST_STD_FS_61",
    "LITEOS_RUST_STD_THREAD_61",
    "LITEOS_RUST_STD_PROCESS_61",
    "LITEOS_RUST_STD_UNIX_61",
    "LITEOS_RUST_STD_IPV4_61",
    "LITEOS_RUST_STD_61",
)


def build_std_smoke(musl: MuslCachePaths, libunwind: Path) -> Path:
    """通过固定 rust-src 与 musl sysroot 构建普通 `fn main` std 动态 PIE。"""
    cargo = shutil.which("cargo")
    rustc = shutil.which("rustc")
    if cargo is None or rustc is None:
        raise RuntimeError("nightly Cargo and rustc are required for Rust std userspace")
    rust_source = rust_source_root(rustc)
    targets = set(run([rustc, "--print", "target-list"], ROOT).splitlines())
    if RUST_USER_TARGET not in targets:
        raise RuntimeError(
            f"rustc lacks {RUST_USER_TARGET}; refusing another architecture or custom target"
        )
    sources = tuple(sorted((CRATE / "src").rglob("*.rs")))
    rust_flags = f"{BASE_RUST_FLAGS} -L native={libunwind.parent}"
    payload = {
        "kind": "rust-std-smoke",
        "recipe_version": 1,
        "arch": TARGET.arch,
        "rust_target": RUST_USER_TARGET,
        "build_std": "std,panic_abort;llvm-libunwind",
        "rustflags": rust_flags,
        "musl_sysroot_fingerprint": musl.sysroot_fingerprint,
        "libunwind_sha256": sha256(libunwind),
        "driver_sha256": sha256(ROOT / "scripts/musl_clang.py"),
        "cargo": run([cargo, "--version"], ROOT).strip(),
        "rustc": run([rustc, "--version"], ROOT).strip(),
        "manifest_sha256": sha256(CRATE / "Cargo.toml"),
        "lock_sha256": sha256(CRATE / "Cargo.lock"),
        "source_sha256": {
            str(source.relative_to(CRATE)): sha256(source) for source in sources
        },
        "std_manifest_sha256": sha256(rust_source / "library/std/Cargo.toml"),
        "unwind_manifest_sha256": sha256(rust_source / "library/unwind/Cargo.toml"),
    }
    identity = fingerprint(payload)
    entry = WORK / "programs" / identity
    if manifest_matches(entry, payload, ("rust-std-smoke",)):
        verify_elf(entry / "rust-std-smoke", musl.compiler, identity="rust-std-smoke")
        return entry / "rust-std-smoke"

    generation = generation_directory(WORK / "program-generations", identity)
    env = build_environment()
    linker_variable = (
        f"CARGO_TARGET_{RUST_USER_TARGET.upper().replace('-', '_')}_LINKER"
    )
    env.update(
        {
            "LITEOS_MUSL_CLANG": str(musl.compiler),
            "LITEOS_MUSL_LLD": str(musl.linker),
            "LITEOS_MUSL_COMPILER_RUNTIME": str(musl.compiler_runtime),
            "LITEOS_MUSL_SYSROOT": str(musl.install),
            "LITEOS_RUST_PROVIDES_COMPILER_BUILTINS": "1",
            "CARGO_INCREMENTAL": "0",
            "CARGO_TARGET_DIR": str(generation / "cargo-target"),
            linker_variable: str(ROOT / "scripts/musl_clang.py"),
            "RUSTFLAGS": rust_flags,
        }
    )
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
                str(CRATE / "Cargo.toml"),
                "--target",
                RUST_USER_TARGET,
                "--release",
                "--locked",
            ],
            ROOT,
            env,
        )
        built = (
            generation
            / "cargo-target"
            / RUST_USER_TARGET
            / "release"
            / "rust-std-smoke"
        )
        if not built.is_file():
            raise RuntimeError("Cargo did not produce rust-std-smoke")
        shutil.copy2(built, generation / "rust-std-smoke")
        verify_elf(
            generation / "rust-std-smoke",
            musl.compiler,
            identity="rust-std-smoke",
        )
        shutil.rmtree(generation / "cargo-target")
        write_manifest(generation, payload)
        publish_generation(generation, entry)
        published = True
    finally:
        if not published:
            shutil.rmtree(generation, ignore_errors=True)
    return entry / "rust-std-smoke"


def install_std_smoke(image: Path, binary: Path, directory: Path) -> None:
    """只向 disposable runtime image 安装 verification-only std consumer。"""
    commands = directory / "rust-std-smoke.debugfs"
    commands.write_text(
        f"write {binary} /bin/rust-std-smoke\n"
        "set_inode_field /bin/rust-std-smoke mode 0100755\n"
    )
    run([str(find_debugfs()), "-w", "-f", str(commands), str(image)], ROOT)


def gate_inputs(image: Path, binary: Path, musl: MuslCachePaths) -> tuple[Path, ...]:
    """返回 std runtime success stamp 绑定的全部目标与执行输入。"""
    artifacts = [
        image,
        binary,
        musl.install / "usr/lib/libc.so",
        ROOT / TARGET.kernel_elf(),
        ROOT / "scripts/musl_clang.py",
        ROOT / "scripts/qemu_gate.py",
        ROOT / "user/base/udhcpc.script",
        Path(__file__).resolve(),
    ]
    kernel_boot = ROOT / TARGET.kernel_boot_artifact()
    if kernel_boot != ROOT / TARGET.kernel_elf():
        artifacts.append(kernel_boot)
    if TARGET.requires_bootloader:
        artifacts.append(
            ROOT
            / "bootloader"
            / "target"
            / TARGET.kernel_triple
            / "release"
            / "bootloader"
        )
    return tuple(artifacts)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--build-only",
        action="store_true",
        help="只构建并校验 Rust std ELF，不创建镜像或启动 QEMU",
    )
    parser.add_argument(
        "--image",
        type=Path,
        default=ROOT / "target" / "rootfs" / f"{TARGET.arch}.img",
        help="只读产品 rootfs baseline；std fixture 只注入临时副本",
    )
    args = parser.parse_args()
    http_server: subprocess.Popen[bytes] | None = None
    try:
        compiler = find_compiler()
        musl = cached_musl_paths(compiler)
        with cache_lock(WORK / ".build.lock"):
            rustc = shutil.which("rustc")
            if rustc is None:
                raise RuntimeError("nightly rustc is required for Rust std userspace")
            libunwind = build_libunwind(musl, rustc)
            binary = build_std_smoke(musl, libunwind)
        if args.build_only:
            print(f"Rust std userspace build passed: {binary}")
            return 0

        image = args.image.resolve()
        if not image.is_file():
            raise RuntimeError(f"rootfs image is missing: {image}")
        stamp = ROOT / "target" / "verify-gates" / f"rust-std-{TARGET.arch}.json"
        payload = runtime_gate_payload(
            "rust-std-runtime",
            1,
            gate_inputs(image, binary, musl),
        )
        if runtime_gate_hit(stamp, payload, (image, binary)):
            print(f"Rust std {TARGET.arch} runtime verification cache hit")
            return 0
        with tempfile.TemporaryDirectory(prefix="liteos-rust-std-gate-") as workspace:
            directory = Path(workspace)
            runtime_image = directory / "fs.img"
            shutil.copyfile(image, runtime_image)
            install_std_smoke(runtime_image, binary, directory)
            http_server, http_port = start_http_gate()
            boot(
                runtime_image,
                1,
                (
                    *cpu_topology_markers(1),
                    "init started: BusyBox v1.37.0",
                    *STD_MARKERS,
                ),
                interactions=(
                    (
                        "Enter 'help' for a list of built-in commands.",
                        ShellCommand(f"/bin/rust-std-smoke {http_port}\n".encode()),
                    ),
                ),
                persistent_writes=True,
                timeout_seconds=60,
            )
        publish_runtime_gate(stamp, payload)
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"Rust std verification failed: {error}", file=sys.stderr)
        return 1
    finally:
        if http_server is not None:
            http_server.terminate()
            try:
                http_server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                http_server.kill()
                http_server.wait(timeout=3)
    print(f"Rust std {TARGET.arch} runtime verification passed")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
