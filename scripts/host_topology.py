#!/usr/bin/env python3
"""计算默认 guest CPU 数，避免把宿主最后一个物理核也交给 QEMU。"""

from __future__ import annotations

import os
import subprocess


def physical_cpu_count() -> int:
    """返回宿主物理 CPU 数；macOS 查询失败时退回在线逻辑 CPU 数。"""
    try:
        result = subprocess.run(
            ["sysctl", "-n", "hw.physicalcpu"],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        count = int(result.stdout.strip())
    except (FileNotFoundError, subprocess.CalledProcessError, ValueError):
        count = os.cpu_count() or 1
    return max(1, count)


def default_guest_cpu_count(host_physical_cpus: int | None = None) -> int:
    """保留一个物理 CPU 给 macOS/QEMU I/O thread，其余 CPU 分配给 guest。"""
    count = physical_cpu_count() if host_physical_cpus is None else host_physical_cpus
    if count <= 0:
        raise ValueError("host physical CPU count must be positive")
    return max(1, count - 1)


if __name__ == "__main__":
    print(default_guest_cpu_count())
