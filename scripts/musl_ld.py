#!/usr/bin/env python3
"""将 BusyBox Kbuild 的可重定位链接请求规范化为 RISC-V LLD 参数。"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


def main() -> int:
    linker = Path(os.environ["LITEOS_MUSL_LLD"])
    clang = Path(os.environ["LITEOS_MUSL_CLANG"])
    arguments = [
        argument
        for argument in sys.argv[1:]
        if not argument.startswith(("-march=", "-mabi="))
    ]
    output = arguments[arguments.index("-o") + 1]
    inputs = [
        argument
        for argument in arguments
        if argument != output and argument.endswith((".o", ".a"))
    ]
    if inputs:
        return subprocess.run(["ld.lld", *arguments], executable=str(linker)).returncode
    return subprocess.run(
        [str(clang), "--target=riscv64-linux-musl", "-c", "-x", "c", "/dev/null", "-o", output]
    ).returncode


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (KeyError, ValueError) as error:
        print(f"musl-ld: invalid linker invocation: {error}", file=sys.stderr)
        raise SystemExit(1)
