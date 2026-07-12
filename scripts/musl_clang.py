#!/usr/bin/env python3
"""为固定 musl sysroot 提供 Linux/RISC-V Clang 编译与动态 PIE 链接入口。"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


def required_path(name: str) -> Path:
    value = os.environ.get(name)
    if value is None:
        raise RuntimeError(f"{name} is required")
    path = Path(value)
    if not path.is_file():
        raise RuntimeError(f"{name} does not name a file: {path}")
    return path


def main() -> int:
    clang = required_path("LITEOS_MUSL_CLANG")
    linker = required_path("LITEOS_MUSL_LLD")
    libgcc = required_path("LITEOS_MUSL_LIBGCC")
    sysroot = Path(os.environ["LITEOS_MUSL_SYSROOT"])
    arguments = sys.argv[1:]
    command = [
        str(clang),
        "--target=riscv64-linux-musl",
        f"--ld-path={linker}",
        "-nostdlibinc",
        "-isystem",
        str(sysroot / "usr/include"),
    ]
    compiling = any(argument in {"-c", "-E", "-S"} for argument in arguments)
    relocatable = "-r" in arguments or any(
        argument.startswith("-Wl,") and "-r" in argument.split(",")[1:]
        for argument in arguments
    )
    output = arguments[arguments.index("-o") + 1] if "-o" in arguments else None
    object_inputs = [
        argument
        for argument in arguments
        if argument != output and argument.endswith((".o", ".a"))
    ]
    if relocatable and not object_inputs:
        return subprocess.run(
            [
                str(clang),
                "--target=riscv64-linux-musl",
                "-c",
                "-x",
                "c",
                "/dev/null",
                "-o",
                output,
            ]
        ).returncode
    if relocatable:
        return subprocess.run(
            ["ld.lld", "-r", "-o", output, *object_inputs], executable=str(linker)
        ).returncode
    if compiling or relocatable:
        command.extend(arguments)
    else:
        library = sysroot / "usr/lib"
        command.extend(
            [
                "-nostdlib",
                str(library / "Scrt1.o"),
                str(library / "crti.o"),
                f"-L{library}",
                *arguments,
                "-lc",
                str(libgcc),
                str(library / "crtn.o"),
                "-Wl,-dynamic-linker,/lib/ld-musl-riscv64.so.1",
            ]
        )
    return subprocess.run(command).returncode


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (KeyError, RuntimeError) as error:
        print(f"musl-clang: {error}", file=sys.stderr)
        raise SystemExit(1)
