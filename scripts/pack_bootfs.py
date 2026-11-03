#!/usr/bin/env python3
"""把系统服务打包为 bootfs（cpio ``newc``），由 QEMU ``-initrd`` 交给内核。

1. 内核从 initrd 取出 ``bin/component-manager`` 并启动它；
2. 根服务再从同一 bootfs 启动 ``bin/driver-manager``（它再按节点启动 ``bin/driver-host``）、``bin/ext4``、``bin/fshost``、``bin/netstack``、
   ``bin/dhcp-client``、``bin/netcfg`` 与 ``bin/linuxd``，并把 ``data/`` 下的只读配置交给对应服务
   （Fuchsia 的 ``/config/data``）；
3. 成员按路径排序、时间戳与 inode 固定，同样的输入总是得到同样的字节。
"""

from __future__ import annotations

import argparse
from pathlib import Path

from build_target import BuildTarget, target_from_environment

ROOT = Path(__file__).resolve().parent.parent

# bootfs 路径 → 构建产物名（cargo bin 名）；内核与根服务按这些路径查找。
SERVICES: tuple[tuple[str, str], ...] = (
    ("bin/driver-manager", "driver-manager"),
    ("bin/driver-host", "driver-host"),
    ("bin/dhcp-client", "dhcp-client"),
    ("bin/ext4", "ext4"),
    ("bin/fshost", "fshost"),
    ("bin/netcfg", "netcfg"),
    ("bin/linuxd", "linuxd"),
    ("bin/netstack", "netstack"),
    ("bin/component-manager", "component-manager"),
)

# bootfs 路径 → 受 Git 管理的只读配置；路径与 `launch_protocol` 中的常量一致。
CONFIG_DATA: tuple[tuple[str, Path], ...] = (
    ("data/netcfg/default.conf", ROOT / "system/netcfg/default.conf"),
)

_REGULAR_EXECUTABLE = 0o100755
_REGULAR_FILE = 0o100644


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
    members = [(path, _REGULAR_EXECUTABLE, service_artifact(target, profile, binary)) for path, binary in SERVICES]
    members += [(path, _REGULAR_FILE, source) for path, source in CONFIG_DATA]
    for inode, (path, mode, source) in enumerate(sorted(members), start=1):
        archive += _member(path, mode, source.read_bytes(), inode)
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
