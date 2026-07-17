#!/usr/bin/env python3
"""Build and run the deterministic LiteOS desktop runtime quality gate."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from build_cache import (
    build_environment,
    fingerprint,
    publish_runtime_gate,
    runtime_gate_hit,
    runtime_gate_payload,
    sha256,
)
from ext2_image import find_debugfs
from qemu_display_gate import run as run_display_gate
from verify_musl import cached_musl_paths, find_compiler

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "scripts/fixtures/desktop"
WORK = ROOT / "target/desktop-runtime"
FORBIDDEN_MARKERS = (
    "unsupported syscall_id:",
    "panicked at",
    "[ERROR]",
    "liteui-compositor: invariant failure",
    "liteui-compositor: display session failed",
    "resize transaction rejected",
    "resize out of memory",
)


def run(command: list[str], env: dict[str, str] | None = None) -> str:
    result = subprocess.run(
        command,
        cwd=ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-80:])
        raise RuntimeError(f"command failed: {' '.join(command)}\n{tail}")
    return result.stdout


def build_inspector() -> Path:
    compiler = find_compiler()
    musl = cached_musl_paths(compiler)
    source = FIXTURES / "liteui-inspect.c"
    identity = fingerprint(
        {
            "kind": "liteui-inspect",
            "recipe_version": 1,
            "source": sha256(source),
            "musl": musl.sysroot_fingerprint,
            "wrapper": sha256(ROOT / "scripts/musl_clang.py"),
        }
    )
    destination = WORK / f"liteui-inspect-{identity}"
    if destination.is_file():
        return destination
    WORK.mkdir(parents=True, exist_ok=True)
    temporary = WORK / f".{destination.name}.{os.getpid()}.tmp"
    env = build_environment()
    env.update(
        {
            "LITEOS_MUSL_CLANG": str(musl.compiler),
            "LITEOS_MUSL_LLD": str(musl.linker),
            "LITEOS_MUSL_LIBGCC": str(musl.libgcc),
            "LITEOS_MUSL_SYSROOT": str(musl.install),
        }
    )
    try:
        run(
            [
                sys.executable,
                str(ROOT / "scripts/musl_clang.py"),
                str(source),
                "-Os",
                "-fPIE",
                "-pie",
                "-Wl,--gc-sections,-z,relro,-z,now,-z,noexecstack",
                "-o",
                str(temporary),
            ],
            env,
        )
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)
    return destination


def inject_gate(base: Path, inspector: Path, directory: Path) -> Path:
    image = directory / "desktop.img"
    shutil.copyfile(base, image)
    transaction = directory / "desktop.debugfs"
    transaction.write_text(
        f"write {inspector} /run/liteui-inspect\n"
        "set_inode_field /run/liteui-inspect mode 0100755\n"
        f"write {FIXTURES / 'verify-desktop.sh'} /run/verify-desktop.sh\n"
        "set_inode_field /run/verify-desktop.sh mode 0100755\n"
    )
    run([str(find_debugfs()), "-w", "-f", str(transaction), str(image)])
    return image


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--image",
        type=Path,
        default=ROOT / "target/rootfs.img",
        help="read-only baseline rootfs; the gate creates private copies",
    )
    parser.add_argument(
        "--build-only",
        action="store_true",
        help="build the guest inspector without launching QEMU",
    )
    args = parser.parse_args()
    image = args.image.resolve()
    try:
        if not image.is_file():
            raise RuntimeError(f"rootfs image is missing: {image}")
        inspector = build_inspector()
        if args.build_only:
            print(f"desktop inspector ready: {inspector}")
            return 0
        inputs = (
            image,
            ROOT / "target/riscv64gc-unknown-none-elf/debug/kernel",
            ROOT / "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
            inspector,
            Path(__file__).resolve(),
            ROOT / "scripts/qemu_display_gate.py",
            ROOT / "scripts/qemu_gate.py",
            ROOT / "scripts/build_cache.py",
            FIXTURES / "liteui-inspect.c",
            FIXTURES / "verify-desktop.sh",
            ROOT / "user/liteui-compositor/src/diagnostics.rs",
            ROOT / "user/liteui-compositor/src/input.rs",
            ROOT / "user/liteui-compositor/src/reactor.rs",
            ROOT / "user/liteui-compositor/src/server.rs",
            ROOT / "user/liteui-compositor/src/server/peer.rs",
        )
        payload = runtime_gate_payload("desktop-runtime", 1, inputs)
        stamp = ROOT / "target/verify-gates/desktop.json"
        if runtime_gate_hit(stamp, payload, (image,)):
            print("desktop runtime verification cache hit")
            return 0
        with tempfile.TemporaryDirectory(prefix="liteos-desktop-image-") as directory:
            gate_image = inject_gate(image, inspector, Path(directory))
            run_display_gate(
                gate_image,
                (
                    "LITEOS_DESKTOP_POINTER_OK",
                    "LITEOS_DESKTOP_DRAG_OK",
                    "LITEOS_DESKTOP_RELEASE_OK",
                    "LITEOS_DESKTOP_KEY_OK",
                    "LITEOS_DESKTOP_RESIZE_OK",
                    "LITEOS_DESKTOP_RSS_OK",
                    "LITEOS_DESKTOP_IDLE_OK",
                    "LITEOS_DESKTOP_RECOVERY_OK",
                    "LITEOS_DESKTOP_RUNTIME_61",
                ),
                FORBIDDEN_MARKERS,
            )
        publish_runtime_gate(stamp, payload)
        print("desktop runtime verification passed")
    except RuntimeError as error:
        print(f"desktop runtime verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
