#!/usr/bin/env python3
"""把系统服务打包为 bootfs（cpio ``newc``），由 QEMU ``-initrd`` 交给内核。

1. 内核从 initrd 取出 ``bin/component-manager`` 并启动它；
2. 根服务再从同一 bootfs 启动 ``bin/driver-manager``（它再按节点启动 ``bin/driver-host``）、``bin/ext4``、``bin/fshost``、``bin/netstack``、
   ``bin/dhcp-client``、``bin/netcfg``、``bin/audio-core`` 与 ``bin/linuxd``，并把 ``data/`` 下的只读配置交给对应服务
   （Fuchsia 的 ``/config/data``）；
3. 成员按路径排序、时间戳与 inode 固定，同样的输入总是得到同样的字节；
4. 文件内容按 4 KiB 页对齐，使装载 ELF 段与交付大文件成为 bootfs VMO 的写时复制子对象而不复制。
"""

from __future__ import annotations

import argparse
import subprocess
import tempfile
from pathlib import Path

from build_target import BuildTarget, target_from_environment
from toolchain.kernel_image import pinned_rust_objcopy

ROOT = Path(__file__).resolve().parent.parent

# bootfs 路径 → 构建产物名（cargo bin 名）；内核与根服务按这些路径查找。
SERVICES: tuple[tuple[str, str], ...] = (
    ("bin/driver-manager", "driver-manager"),
    ("bin/driver-host", "driver-host"),
    ("bin/audio-core", "audio-core"),
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


PAGE = 4096
_HEADER = 110


def _member(name: str, mode: int, data: bytes, inode: int, start: int = 0) -> bytes:
    """编码一个 cpio ``newc`` 成员。

    ``start`` 是该成员在归档中的起始偏移。非空文件的内容按页对齐：多余的填充放进路径名字段
    （NUL 之后的 NUL），这是标准 cpio 读取器都接受的写法。内容页对齐让 ELF 段与大文件
    成为 bootfs VMO 的写时复制子对象，装载时不复制字节。
    """
    encoded = name.encode() + b"\0"
    if data:
        padding = -(start + _HEADER + len(encoded)) % PAGE
    else:
        padding = -(start + _HEADER + len(encoded)) % 4
    encoded += b"\0" * padding
    fields = (inode, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(encoded), 0)
    header = b"070701" + b"".join(f"{field:08x}".encode() for field in fields)
    member = header + encoded
    assert not data or (start + len(member)) % PAGE == 0
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
        subprocess.CalledProcessError: ELF 副本剥离失败。
    """
    archive = bytearray()
    members = [(path, _REGULAR_EXECUTABLE, service_artifact(target, profile, binary)) for path, binary in SERVICES]
    members += [(path, _REGULAR_FILE, source) for path, source in CONFIG_DATA]
    objcopy = pinned_rust_objcopy()
    # 只剥离归档副本的符号/调试数据，保留装载段与生命周期 PT_NOTE；宿主 ELF 仍供调试和热更新使用。
    with tempfile.TemporaryDirectory(prefix="liteos-bootfs-") as temporary:
        stripped = Path(temporary) / "service"
        for inode, (path, mode, source) in enumerate(sorted(members), start=1):
            if mode == _REGULAR_EXECUTABLE:
                source.stat()
                subprocess.run([str(objcopy), "--strip-all", str(source), str(stripped)], check=True)
                data = stripped.read_bytes()
            else:
                data = source.read_bytes()
            archive += _member(path, mode, data, inode, len(archive))
    archive += _member("TRAILER!!!", 0, b"", 0, len(archive))
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
