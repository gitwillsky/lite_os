#!/usr/bin/env python3
"""获取 curl、SQLite、Git 竖切所需的固定 Alpine APK 闭包。"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from apk_cache import ALPINE_ARCH, ALPINE_BRANCH, WORK, download
from build_cache import cache_lock, fingerprint

_RISCV64_APPLICATION_PACKAGES = (
    ("brotli-libs-1.1.0-r2.apk", "692bbfa741115c9f3bebfb6779b837aad2c9bf7b5eb15de2a65499b25c1622fe"),
    ("c-ares-1.34.8-r0.apk", "5e747174a2f321c8561b96a1e74198678c16ac4bc93a6834564e66b302ec07a6"),
    ("curl-8.14.1-r3.apk", "382c2e531b0e9d44a0a4dd5b32ed4a5d2b89ab8ba04f88ba29b0125006b76b6b"),
    ("git-2.49.1-r0.apk", "435aab6e568dd0fc9d128214b91aae1582dfc6796ae70a2f6f90747cd18e48ef"),
    ("git-init-template-2.49.1-r0.apk", "3dc4ae199efaa633c5931d6aef8b6f4c23ea0a74ec66b43b8a47602f9c640951"),
    ("libcrypto3-3.5.9-r0.apk", "2bb9af0e10adf4620074c9315c0db57dcd8b1c0492172d6782f59bb20e122cd0"),
    ("libcurl-8.14.1-r3.apk", "3bee58fbf961ebe022092c0f2cb28fd52aa4d7376cae006dab2a51e4ef8f6f06"),
    ("libexpat-2.8.5-r0.apk", "a969adc4feb1b32669cebf993467270f48980f73beceb13375e8b200be1dcfda"),
    ("libidn2-2.3.7-r0.apk", "5a956535be25c226dfbde8dc51126944a41436ce888fa9d5ea130993969462e3"),
    ("libncursesw-6.5_p20250503-r0.apk", "7d88384bd34c276bafcaae52289e62b895bc5265f74369dc0ad523ef204d9607"),
    ("libpsl-0.21.5-r3.apk", "82e4cfade01321a9746764c7999a3f2193b2defbed5defe9de30004fe5422c75"),
    ("libssl3-3.5.9-r0.apk", "528e80a3ef436e662adb66be1638454cc435c28cf4df678eb17332e99d0c2ec5"),
    ("libunistring-1.3-r0.apk", "76ee5ab5fc880db1498ae2d8481981d323af67e04aa7591ca3fbda6a86587cb8"),
    ("ncurses-terminfo-base-6.5_p20250503-r0.apk", "8e523158d782d13d311fffde76d891812be9e4e0d5b4e30dd14ad1c21848243a"),
    ("nghttp2-libs-1.69.0-r0.apk", "3d554fbada551f44c8e1dda4581cb531789e7ebd80a6f3aaf97e0e95280d993d"),
    ("pcre2-10.46-r0.apk", "110db96f56a91a4513ed67c511d4aec173915c2a4d2ba169f0cfe9305278a619"),
    ("readline-8.2.13-r1.apk", "4b6c7e5fd085d21aa6c2f527aec4a782111dc462615213953d9c44753e7e05fa"),
    ("sqlite-3.49.2-r1.apk", "0eebce26283fb9d6254d37ecce0b20964f6746bda1518dfd9ae0391923bbd054"),
    ("zlib-1.3.2-r0.apk", "9a2761a457312f4aa1312c94d3ca8789c2f1dd51d34d992e400851c8181a6887"),
    ("zstd-libs-1.5.7-r0.apk", "28bb837f617870a996009130c938ad12075f942738cfcd1390251720c78f0b8d"),
)
_APPLICATION_PACKAGES_BY_ARCH: dict[str, tuple[tuple[str, str], ...]] = {
    "riscv64": _RISCV64_APPLICATION_PACKAGES,
    "aarch64": (
        ("brotli-libs-1.1.0-r2.apk", "b05f9d2839bb89f28325890ffbd7d94025af6b301344ae0255f08617a6036c65"),
        ("c-ares-1.34.8-r0.apk", "e293f056615fff3cf6050a423c090179daf48516317912daf1e133a1095a2f65"),
        ("curl-8.14.1-r3.apk", "795f36d5c69d6e1a4dcdbf044da4511aa14c3122b33905feeb93e587ec01acc7"),
        ("git-2.49.1-r0.apk", "9d27f0e5e57b234e6d6a8a2b635ba82286e1dd441ea26c280aba4c8e416b4f60"),
        ("git-init-template-2.49.1-r0.apk", "94e1f133470fdc35e24304902c88a3bcc8178cdab24c8190edabf89f804f0984"),
        ("libcrypto3-3.5.9-r0.apk", "ede2ef7eca3b65a42e902aaf7890f9c5647dca0c55123a817179ad9f3bc6ef24"),
        ("libcurl-8.14.1-r3.apk", "a811d6f0370383461ed4993434f84b812f29a48009332f517d8a373613797532"),
        ("libexpat-2.8.5-r0.apk", "24f367fc510fd8be0591f6b39daf16da2d13257e03d97b5a6505ae44ba82e978"),
        ("libidn2-2.3.7-r0.apk", "9ddc248988707da96752077d05bacbda751d46cc1f7aa1460b3a40c4fbf66a6e"),
        ("libncursesw-6.5_p20250503-r0.apk", "419b375e8a4345e7172b1f0f3a3c57db61374f5408cdb875d9e860bd4c243aca"),
        ("libpsl-0.21.5-r3.apk", "b9f7270cb2980876f57360f38ca11093aefaf7f5eb4b777dfbc4e5738449e438"),
        ("libssl3-3.5.9-r0.apk", "338c9f6154582b4b76455a59f95538bcea8df039c5c72916dd2d8d0d844c9583"),
        ("libunistring-1.3-r0.apk", "6b6631284b25fa28bb9e63f9d423145e96d0f7aeb55a592f5cd5d54fb39380f7"),
        ("ncurses-terminfo-base-6.5_p20250503-r0.apk", "3d37403e0b5ab9eb0c1ce269444e4a385faec9fe6af452c1c6956806b13d2bd6"),
        ("nghttp2-libs-1.69.0-r0.apk", "19db967a36f1e041e96240484d94063354f5c837e36d50daa017c2e96393a5e7"),
        ("pcre2-10.46-r0.apk", "62fdc4a3d6b48ca211cf6480c5da55664b489ec2b192ca8942e5b1d60ebe9496"),
        ("readline-8.2.13-r1.apk", "334af29dbf6b5a71a87af4d6a58e2967a8f711a51d00093de0e1498daf83ceb2"),
        ("sqlite-3.49.2-r1.apk", "23cc7ebfee1170d2e6be5740ef5eae1c522691e164e399629f9147705370e8c9"),
        ("zlib-1.3.2-r0.apk", "7a39a917e4dab3c7a45537210ee5b5f17bf75f5e7777809a20cddd0afe074187"),
        ("zstd-libs-1.5.7-r0.apk", "a0e92d2225941a514eb0b2325b137fe6444ef9171627aae8129b74a6ad934ac4"),
    ),
}


@dataclass(frozen=True)
class ApplicationApks:
    """@description 已校验的完整应用 APK 闭包与内容身份。"""

    archives: tuple[Path, ...]
    fingerprint: str


def fixed_application_packages() -> tuple[tuple[str, str], ...]:
    """返回当前架构固定闭包；摘要不完整时禁止任何下载。"""
    packages = _APPLICATION_PACKAGES_BY_ARCH[ALPINE_ARCH]
    if packages:
        return packages
    missing = ", ".join(name for name, _ in _RISCV64_APPLICATION_PACKAGES)
    raise RuntimeError(
        f"fixed Alpine {ALPINE_BRANCH} application SHA-256 values are missing "
        f"for {ALPINE_ARCH}: {missing}"
    )


def cached_application_apks() -> ApplicationApks:
    """下载或复用固定包闭包；任一 SHA-256 不符都会 fail-stop。"""
    packages = fixed_application_packages()
    with cache_lock(WORK / ".applications.lock"):
        archives = tuple(download(name, digest).resolve() for name, digest in packages)
        identity = fingerprint(
            {
                "kind": "apk-application-closure",
                "branch": ALPINE_BRANCH,
                "arch": ALPINE_ARCH,
                "packages": dict(packages),
            }
        )
        return ApplicationApks(archives=archives, fingerprint=identity)
