#!/usr/bin/env python3
"""为固定 musl sysroot 提供目标相关 Clang 编译与动态 PIE 链接入口。"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from build_target import target_from_environment


def required_path(name: str) -> Path:
    value = os.environ.get(name)
    if value is None:
        raise RuntimeError(f"{name} is required")
    path = Path(value)
    if not path.is_file():
        raise RuntimeError(f"{name} does not name a file: {path}")
    return path


def main() -> int:
    target = target_from_environment()
    target_argument = f"--target={target.linux_triple}"
    clang = required_path("LITEOS_MUSL_CLANG")
    linker = required_path("LITEOS_MUSL_LLD")
    compiler_runtime = required_path("LITEOS_MUSL_COMPILER_RUNTIME")
    sysroot = Path(os.environ["LITEOS_MUSL_SYSROOT"])
    arguments = sys.argv[1:]
    query = {"--version", "-dumpmachine", "-dumpversion", "-print-search-dirs"}
    if arguments and all(argument in query for argument in arguments):
        return subprocess.run(
            [str(clang), target_argument, *arguments]
        ).returncode
    command = [
        str(clang),
        target_argument,
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
                target_argument,
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
    if not compiling:
        command.insert(2, f"--ld-path={linker}")
    if compiling or relocatable:
        command.extend(arguments)
    else:
        library = sysroot / "usr/lib"
        if "-shared" in arguments:
            command.extend(
                [
                    "-nostdlib",
                    f"-L{library}",
                    *arguments,
                    "-lc",
                    str(compiler_runtime),
                ]
            )
        else:
            command.extend(
                [
                    "-nostdlib",
                    str(library / "Scrt1.o"),
                    str(library / "crti.o"),
                    f"-L{library}",
                    *arguments,
                    "-lc",
                    str(compiler_runtime),
                    str(library / "crtn.o"),
                    f"-Wl,-dynamic-linker,{target.musl_loader}",
                ]
            )
    return subprocess.run(command).returncode


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (KeyError, RuntimeError) as error:
        print(f"musl-clang: {error}", file=sys.stderr)
        raise SystemExit(1)
