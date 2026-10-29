"""构建产物与启动镜像契约门禁。"""
from __future__ import annotations

import argparse
import struct
import subprocess
import sys
from build_target import BuildTarget, target_from_environment
from dataclasses import dataclass
from pathlib import Path
from toolchain.kernel_image import (
    build_kernel_boot_artifact,
    inspect,
    llvm_tool,
    require,
    require_aarch64_elf_contract,
    require_aarch64_raw_image,
)
from toolchain.musl import find_compiler
from userland.busybox import cached_busybox_binary

ROOT = Path(__file__).resolve().parent.parent
TARGET = target_from_environment()


@dataclass(frozen=True)
class ArtifactSpec:
    path: Path
    markers: tuple[str, ...]


def target_artifacts(target: BuildTarget, busybox: Path) -> tuple[ArtifactSpec, ...]:
    """Return the exact release products owned by one selected target."""
    machine = "AArch64" if target.arch == "aarch64" else "RISC-V"
    artifacts = [
        ArtifactSpec(
            ROOT / target.kernel_elf(),
            ("ELF64", machine, "EXEC"),
        )
    ]
    if target.requires_bootloader:
        artifacts.append(
            ArtifactSpec(
                ROOT
                / "bootloader/target"
                / target.kernel_triple
                / "release/bootloader",
                ("ELF64", "RISC-V", "EXEC"),
            )
        )
    artifacts.append(
        ArtifactSpec(
            busybox,
            (
                "ELF64",
                machine,
                "DYN (",
                "INTERP",
                "DYNAMIC",
                "GNU_RELRO",
                "GNU_STACK",
                target.musl_loader,
            ),
        )
    )
    return tuple(artifacts)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-boot-artifact", action="store_true")
    parser.add_argument("--profile", choices=("release", "debug"), default="release")
    arguments = parser.parse_args()
    try:
        target = target_from_environment()
        if arguments.build_boot_artifact:
            artifact = build_kernel_boot_artifact(target, arguments.profile)
            print(f"kernel boot artifact ready: {artifact.relative_to(ROOT)}")
            return 0
        readelf = llvm_tool("llvm-readelf")
        busybox = cached_busybox_binary(find_compiler())
        artifacts = target_artifacts(target, busybox)
        for artifact in artifacts:
            label = (
                str(artifact.path.relative_to(ROOT))
                if artifact.path.is_relative_to(ROOT)
                else str(artifact.path)
            )
            require(inspect(readelf, artifact.path), label, artifact.markers)
        if target.arch == "aarch64":
            kernel = artifacts[0].path
            nm_output = subprocess.run(
                [llvm_tool("llvm-nm"), "--defined-only", str(kernel)],
                check=True,
                cwd=ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            ).stdout
            require_aarch64_elf_contract(kernel, nm_output)
            require_aarch64_raw_image(
                kernel,
                ROOT / target.kernel_boot_artifact(),
            )
    except (RuntimeError, OSError, struct.error, subprocess.CalledProcessError) as error:
        print(f"artifact verification failed: {error}", file=sys.stderr)
        return 1
    print("artifact verification passed")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
