#!/usr/bin/env python3
"""离线扩容开发用 ext 镜像，并保留镜像中的现有文件。"""

from __future__ import annotations

import argparse
from pathlib import Path

from ext2_image import ensure_ext2_capacity, ext2_capacity_bytes


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, required=True, help="待扩容的 ext 镜像")
    parser.add_argument(
        "--size-mib",
        type=int,
        required=True,
        help="开发镜像最小容量（MiB）；已有更大镜像不会缩容",
    )
    args = parser.parse_args()
    before = ext2_capacity_bytes(args.image)
    ensure_ext2_capacity(args.image, args.size_mib)
    after = ext2_capacity_bytes(args.image)
    if after == before:
        print(f"rootfs capacity unchanged: {after // 1024 // 1024} MiB")
    else:
        print(
            "rootfs capacity expanded: "
            f"{before // 1024 // 1024} MiB -> {after // 1024 // 1024} MiB"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
