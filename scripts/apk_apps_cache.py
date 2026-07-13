#!/usr/bin/env python3
"""获取 curl、SQLite、Git 竖切所需的固定 Alpine APK 闭包。"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from apk_cache import ALPINE_ARCH, ALPINE_BRANCH, download
from build_cache import fingerprint

APPLICATION_PACKAGES = (
    ("brotli-libs-1.1.0-r2.apk", "692bbfa741115c9f3bebfb6779b837aad2c9bf7b5eb15de2a65499b25c1622fe"),
    ("c-ares-1.34.8-r0.apk", "5e747174a2f321c8561b96a1e74198678c16ac4bc93a6834564e66b302ec07a6"),
    ("curl-8.14.1-r2.apk", "ba8b8cac26aea43629cda8ffebedaf5885f4f03445abddb7506bdb4a18c158b1"),
    ("git-2.49.1-r0.apk", "435aab6e568dd0fc9d128214b91aae1582dfc6796ae70a2f6f90747cd18e48ef"),
    ("git-init-template-2.49.1-r0.apk", "3dc4ae199efaa633c5931d6aef8b6f4c23ea0a74ec66b43b8a47602f9c640951"),
    ("libcrypto3-3.5.7-r0.apk", "ae078b6ab8428ac5ed383e4ce33042c4d0410db7f0230fe85c3c2ab74f473159"),
    ("libcurl-8.14.1-r2.apk", "7ddee80c3cdd98fbb01129d529142314dd3da33efce6172834bb39cb6aa213c0"),
    ("libexpat-2.8.2-r0.apk", "db72a94350d5d3a833a3f45cd992796a49d3770fa002ffa1a3c80181c05ec76d"),
    ("libidn2-2.3.7-r0.apk", "5a956535be25c226dfbde8dc51126944a41436ce888fa9d5ea130993969462e3"),
    ("libncursesw-6.5_p20250503-r0.apk", "7d88384bd34c276bafcaae52289e62b895bc5265f74369dc0ad523ef204d9607"),
    ("libpsl-0.21.5-r3.apk", "82e4cfade01321a9746764c7999a3f2193b2defbed5defe9de30004fe5422c75"),
    ("libssl3-3.5.7-r0.apk", "06dce38050691be9519e4ffb4ed274a917f1d7934b24e4ca65f8640d15c7943e"),
    ("libunistring-1.3-r0.apk", "76ee5ab5fc880db1498ae2d8481981d323af67e04aa7591ca3fbda6a86587cb8"),
    ("ncurses-terminfo-base-6.5_p20250503-r0.apk", "8e523158d782d13d311fffde76d891812be9e4e0d5b4e30dd14ad1c21848243a"),
    ("nghttp2-libs-1.69.0-r0.apk", "3d554fbada551f44c8e1dda4581cb531789e7ebd80a6f3aaf97e0e95280d993d"),
    ("pcre2-10.46-r0.apk", "110db96f56a91a4513ed67c511d4aec173915c2a4d2ba169f0cfe9305278a619"),
    ("readline-8.2.13-r1.apk", "4b6c7e5fd085d21aa6c2f527aec4a782111dc462615213953d9c44753e7e05fa"),
    ("sqlite-3.49.2-r1.apk", "0eebce26283fb9d6254d37ecce0b20964f6746bda1518dfd9ae0391923bbd054"),
    ("zlib-1.3.2-r0.apk", "9a2761a457312f4aa1312c94d3ca8789c2f1dd51d34d992e400851c8181a6887"),
    ("zstd-libs-1.5.7-r0.apk", "28bb837f617870a996009130c938ad12075f942738cfcd1390251720c78f0b8d"),
)


@dataclass(frozen=True)
class ApplicationApks:
    """@description 已校验的完整应用 APK 闭包与内容身份。"""

    archives: tuple[Path, ...]
    fingerprint: str


def cached_application_apks() -> ApplicationApks:
    """下载或复用固定包闭包；任一 SHA-256 不符都会 fail-stop。"""
    archives = tuple(download(name, digest).resolve() for name, digest in APPLICATION_PACKAGES)
    identity = fingerprint(
        {
            "kind": "apk-application-closure",
            "branch": ALPINE_BRANCH,
            "arch": ALPINE_ARCH,
            "packages": dict(APPLICATION_PACKAGES),
        }
    )
    return ApplicationApks(archives=archives, fingerprint=identity)
