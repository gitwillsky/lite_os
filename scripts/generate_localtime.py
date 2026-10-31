#!/usr/bin/env python3
"""生成 `user/base/localtime`：固定偏移（UTC+8，CST）的 TZif v1 文件，作为新镜像 `/etc/localtime` 的默认时区。

TZif 是 libc（musl）读取本地时区的标准格式，所以所有程序（BusyBox `date`、桌面时钟）自动采用它；
控制面板"日期/时间 属性"的"时区"页写的是同一格式的同一个文件。
"""

import struct
from pathlib import Path

OFFSET_SECONDS = 8 * 3600
ABBREVIATION = b"CST\0"


def tzif(offset: int, abbreviation: bytes) -> bytes:
    """固定偏移、无夏令时、无转换点的 TZif v1。"""
    header = b"TZif" + b"\0" + b"\0" * 15
    header += struct.pack(">6l", 0, 0, 0, 0, 1, len(abbreviation))  # isutc isstd leap time type char
    return header + struct.pack(">lBB", offset, 0, 0) + abbreviation


if __name__ == "__main__":
    out = Path(__file__).resolve().parents[1] / "user/base/localtime"
    out.write_bytes(tzif(OFFSET_SECONDS, ABBREVIATION))
    print(f"wrote {out}")
