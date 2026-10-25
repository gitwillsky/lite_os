#!/usr/bin/env python3
"""安装固定的原生 Rust/Cargo 与链接器，复用现有 APK 管理和 musl-devel owner。"""
from __future__ import annotations
import argparse
import fcntl
import json
import subprocess
import tempfile
from pathlib import Path
from apk_cache import download
from build_cache import fingerprint, sha256
from build_target import target_from_environment
from ext4_image import ensure_ext4_capacity, find_debugfs, recover_ext4_journal
from qemu_gate import boot, guest_inittab

ROOT = Path(__file__).resolve().parent.parent
# 从 Alpine v3.22/aarch64 官方索引固定的 Rust 1.87.0 闭包。
# musl/musl-dev 由 LiteOS 基础包提供，不能用上游 APK 覆盖系统 libc 与开发头文件。
PACKAGES = (
    ('binutils-2.44-r3.apk', '4e08a500a402ffef9bc9bd4adf4fea5ff72446767b59f0177df6de46708a3b52'),
    ('brotli-libs-1.1.1-r0.apk', 'c4ecb26f02ab93c7e41c019c98bf024bd6d90593be308a003bc516806732500e'),
    ('c-ares-1.34.9-r0.apk', '9ff2336ef422f2278e0f854ab914c5115747e5621205f85b4d1290c755923e05'),
    ('ca-certificates-bundle-20261020-r0.apk', 'eb15a8dba0e6ae8c540d172836d44fe058d6c64adc4a2fca5d2c6eaffa015f1c'),
    ('cargo-1.87.0-r1.apk', 'bed7a6f06ef330ebd206c4607418d96683af1fe2c7c82f8fd3675b31e9e3ec47'),
    ('gcc-14.2.0-r6.apk', '0224b985a055ef90324dee9a65c5683f28499f0798db23cae90b40ebba620772'),
    ('gmp-6.3.0-r3.apk', '0d2eb1079b1b5692e9e6652ff0e269caeb9c812f483e34c88d461c03bcf75460'),
    ('isl25-0.25-r2.apk', 'f9c07b60bdf18754b2279e9f0fe0f7afb166bc18a322c55aa7af4eb3bd7c2eb8'),
    ('jansson-2.14.1-r0.apk', 'f57c3bceb823add72ee0ccbf17aa7fc98596ff9eb0c11d0e5a2c28260ea87dcd'),
    ('libatomic-14.2.0-r6.apk', '7b4427d24bf2e3418a1a3ea97eef81aedbe7aa060ad186ad5387b3a597931c4e'),
    ('libcrypto3-3.5.9-r0.apk', 'ede2ef7eca3b65a42e902aaf7890f9c5647dca0c55123a817179ad9f3bc6ef24'),
    ('libcurl-8.14.1-r4.apk', '99768d3f61dc96ac3b35049a0df4107d9cc6c51e118e865628fa8fa781e0b419'),
    ('libffi-3.4.8-r0.apk', '9391f60a14c146655deaf65115563bc8dcd749cf0f93ec567e6443f2ed7d3bfc'),
    ('libgcc-14.2.0-r6.apk', 'ba1835eec3ad8a120efd3d5020e561d53553a0513763a08f509e3ce6d4baa9ca'),
    ('libgomp-14.2.0-r6.apk', '97a2f9a89d8b3cc7714e1d5a0ab2e01bce711e8a3b330db36b5f876d9aa4ec8c'),
    ('libidn2-2.3.7-r0.apk', '9ddc248988707da96752077d05bacbda751d46cc1f7aa1460b3a40c4fbf66a6e'),
    ('libpsl-0.21.5-r3.apk', 'b9f7270cb2980876f57360f38ca11093aefaf7f5eb4b777dfbc4e5738449e438'),
    ('libssl3-3.5.9-r0.apk', '338c9f6154582b4b76455a59f95538bcea8df039c5c72916dd2d8d0d844c9583'),
    ('libstdc++-14.2.0-r6.apk', '0d2f054057a4f932e985a129eccb79908b40964185139a0a609aed3032aba064'),
    ('libunistring-1.3-r0.apk', '6b6631284b25fa28bb9e63f9d423145e96d0f7aeb55a592f5cd5d54fb39380f7'),
    ('libxml2-2.13.9-r1.apk', 'e358e1c32ba6923386263dd641a6e4bcb56a0345fecadef9ef1103892cf3d81d'),
    ('llvm20-libs-20.1.8-r0.apk', 'c4fa059fd8ccd4d35459b05124ef168ddb9837ceb4ccc3b5cdcc2e03c46ae706'),
    ('mpc1-1.3.1-r1.apk', '20dca4b0a808aef7d5d9e59eaa1dea59b929f65b103107a1396acab81c2ad278'),
    ('mpfr4-4.2.1_p1-r0.apk', '258d57fa9c59eee21110a2d3f44a96f893a132e77febb7a5c0abca8512c6fc30'),
    ('nghttp2-libs-1.69.0-r0.apk', '19db967a36f1e041e96240484d94063354f5c837e36d50daa017c2e96393a5e7'),
    ('rust-1.87.0-r1.apk', '1239833312acea0c10aed2b26df2ccce8878283acc50b643b87afc579d326122'),
    ('scudo-malloc-20.1.8-r0.apk', 'f76505952e7315287163830a55e28f5cc7e03f6eaf85d98123f235d6dc844f80'),
    ('xz-libs-5.8.4-r0.apk', '8a64b55223f29e24f934ccbc8167d535b913bf923dab5e14de7946a7c36091a7'),
    ('zlib-1.3.2-r1.apk', '2d274cb1b2a36d0e8347d802e6dea8648cab952ea5fae1989ab72e725842a2c3'),
    ('zstd-libs-1.5.7-r0.apk', 'a0e92d2225941a514eb0b2325b137fe6444ef9171627aae8129b74a6ad934ac4'),
)
INSTALL = ROOT / "scripts/rust-development/install.sh"
STAMP = "/usr/share/liteos/rust-development.json"


def batch(image: Path, directory: Path, commands: list[str]) -> str:
    """在已锁定镜像执行 debugfs batch；错误保留输出。"""
    script = directory / "rust-development.debugfs"
    script.write_text("\n".join(commands) + "\n")
    result = subprocess.run([str(find_debugfs()), "-w", "-f", str(script), str(image)], check=True, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    return result.stdout


def prepare(image: Path, size: int) -> bool:
    """安装 AArch64 原生工具链；镜像占用、输入或 APK 安装失败直接返回错误。

    Returns: 新安装返回 True，相同内容身份返回 False。
    """
    if target_from_environment().arch != "aarch64":
        raise RuntimeError("native Rust development currently requires ARCH=aarch64")
    archives = tuple(download(name, digest) for name, digest in PACKAGES)
    payload = {"kind": "rust-development", "recipe": 1, "packages": dict(PACKAGES), "installer": sha256(INSTALL)}
    identity = fingerprint(payload)
    with image.open("r+b") as stream:
        try:
            fcntl.lockf(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise RuntimeError(f"development image is already in use: {image}") from error
        recover_ext4_journal(image)
        ensure_ext4_capacity(image, size)
        with tempfile.TemporaryDirectory(prefix="liteos-rust-install-") as temporary:
            directory = Path(temporary)
            observed = directory / "installed.json"
            batch(image, directory, [f"dump {STAMP} {observed}"])
            if observed.is_file() and json.loads(observed.read_text()).get("identity") == identity:
                print(f"Rust development package cache hit: {identity[:12]}")
                return False
            normal = directory / "normal.inittab"
            batch(image, directory, [f"dump /var/tmp/liteos-rust/normal.inittab {normal}"])
            if not normal.is_file():
                batch(image, directory, [f"dump /etc/inittab {normal}"])
            if not normal.is_file():
                raise RuntimeError("cannot preserve normal init policy")
            stamp = directory / "stamp.json"
            stamp.write_text(json.dumps({"identity": identity, "payload": payload}, sort_keys=True) + "\n")
            bootstrap = directory / "bootstrap.inittab"
            bootstrap.write_text(guest_inittab("/bin/sh /var/tmp/liteos-rust/install.sh"))
            commands = ["mkdir /var/tmp/liteos-rust", "mkdir /var/tmp/liteos-rust/apks"]
            for source, destination in [(normal, "/var/tmp/liteos-rust/normal.inittab"), (stamp, "/var/tmp/liteos-rust/stamp.json"), (INSTALL, "/var/tmp/liteos-rust/install.sh")]:
                commands.extend((f"rm {destination}", f"write {source} {destination}"))
            for archive in archives:
                commands.extend((f"rm /var/tmp/liteos-rust/apks/{archive.name}", f"write {archive} /var/tmp/liteos-rust/apks/{archive.name}"))
            commands.extend(("rm /etc/inittab", f"write {bootstrap} /etc/inittab"))
            batch(image, directory, commands)
    # 仅执行 APK 安装事务，不启动 GUI，也不把编译/运行 smoke 混入安装入口。
    boot(image, 4, ("LITEOS_RUST_PACKAGES_INSTALLED",), timeout_seconds=300, persistent_writes=True, memory="6G", success_settle_seconds=1)
    recover_ext4_journal(image)
    print("Installed native Rust/Cargo 1.87.0 and linker packages")
    return True


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--size-mib", type=int, default=8192)
    args = parser.parse_args()
    prepare(args.image.resolve(), args.size_mib)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
