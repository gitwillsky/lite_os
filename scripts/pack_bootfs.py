#!/usr/bin/env python3
"""把系统服务打包为 bootfs（cpio ``newc``），由 QEMU ``-initrd`` 交给内核。

1. 内核从 initrd 取出 ``bin/root-server`` 并启动它；
2. 根服务再从同一 bootfs 启动 ``bin/devmgr``、``bin/ext4d`` 与 ``bin/linuxd``；
3. 成员按路径排序、时间戳与 inode 固定，同样的输入总是得到同样的字节。
"""

from __future__ import annotations

import argparse
from pathlib import Path

from build_target import BuildTarget, target_from_environment

ROOT = Path(__file__).resolve().parent.parent

# bootfs 路径 → 构建产物名（cargo bin 名）；内核与根服务按这些路径查找。
SERVICES: tuple[tuple[str, str], ...] = (
    ("bin/devmgr", "devmgr"),
    ("bin/ext4d", "ext4d"),
    ("bin/linuxd", "linuxd"),
    ("bin/root-server", "root-server"),
)

_REGULAR_EXECUTABLE = 0o100755


def _member(name: str, mode: int, data: bytes, inode: int) -> bytes:
    encoded = name.encode() + b"\0"
    fields = (inode, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(encoded), 0)
    header = b"070701" + b"".join(f"{field:08x}".encode() for field in fields)
    member = header + encoded
    member += b"\0" * (-len(member) % 4)
    member += data
    member += b"\0" * (-len(member) % 4)
    return member


def service_artifact(target: BuildTarget, profile: str, binary: str) -> Path:
    """返回一个系统服务的构建产物路径。"""
    return ROOT / "target" / target.service_triple / profile / binary


def pack(target: BuildTarget, profile: str) -> Path:
    """写出 bootfs 并返回其路径。

    Raises:
        FileNotFoundError: 某个服务尚未构建。
    """
    archive = bytearray()
    for inode, (path, binary) in enumerate(sorted(SERVICES), start=1):
        archive += _member(path, _REGULAR_EXECUTABLE, service_artifact(target, profile, binary).read_bytes(), inode)
    archive += _member("TRAILER!!!", 0, b"", 0)
    output = ROOT / target.bootfs(profile)
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_name(f".{output.name}.tmp")
    temporary.write_bytes(bytes(archive))
    temporary.replace(output)
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=("release", "debug"), default="release")
    arguments = parser.parse_args()
    print(pack(target_from_environment(), arguments.profile))


if __name__ == "__main__":
    main()
