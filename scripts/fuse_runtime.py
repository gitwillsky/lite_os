"""固定 Alpine fusermount3，保留 upstream setuid 入口；不安装第二套 FUSE 客户端库。"""
from __future__ import annotations
import shutil
import subprocess
import tarfile
from pathlib import Path
from apk_cache import download
from build_cache import cache_lock, fingerprint, manifest_matches, publish_directory, temporary_directory, write_manifest
from build_target import target_from_environment

PACKAGES = {
    "aarch64": "cb77007fa83b6e75a6da20aecab37c79a62f3aee25958eba7b3b382bf8bcc099",
    "riscv64": "080203debb4f1751bcd44f8588abd9670343d40441b12ddc656e21d110a0e1bd",
}

def fuse_userland():
    """返回只依赖当前 musl 的原版 helper 与禁止 allow_other 的配置；摘要/ELF 不符立即失败。"""
    from verify_busybox import UserlandArtifact
    from verify_artifacts import llvm_tool
    root = Path(__file__).resolve().parent.parent
    target = target_from_environment()
    name = "fuse3-3.16.2-r1.apk"
    payload = {"kind": "fusermount3", "recipe": 1, "arch": target.arch, "apk": name, "sha256": PACKAGES[target.arch]}
    cache = root / "target/fuse-runtime" / target.arch
    final = cache / fingerprint(payload)
    with cache_lock(cache / ".lock"):
        if not manifest_matches(final, payload, ("fusermount3",)):
            temporary = temporary_directory(cache, "fusermount3")
            try:
                with tarfile.open(download(name, PACKAGES[target.arch]), "r:gz") as archive:
                    member = archive.getmember("usr/bin/fusermount3")
                    if not member.isfile() or member.mode & 0o4777 != 0o4755:
                        raise RuntimeError("upstream fusermount3 must be a regular setuid executable")
                    with archive.extractfile(member) as stream, (temporary / "fusermount3").open("wb") as output:
                        shutil.copyfileobj(stream, output)
                metadata = subprocess.check_output([str(llvm_tool("llvm-readelf")), "--file-header", "--dynamic", str(temporary / "fusermount3")], text=True)
                machine = "AArch64" if target.arch == "aarch64" else "RISC-V"
                needed = [line.split("[")[1].split("]")[0] for line in metadata.splitlines() if "(NEEDED)" in line]
                if machine not in metadata or needed != [Path(target.musl_loader).name.replace("ld-musl-", "libc.musl-")]:
                    raise RuntimeError(f"unexpected fusermount3 ELF dependencies: {needed}")
                write_manifest(temporary, payload)
                publish_directory(temporary, final)
            finally:
                shutil.rmtree(temporary, ignore_errors=True)
    return (UserlandArtifact(final / "fusermount3", "/bin/fusermount3", 0o4755), UserlandArtifact(root / "user/base/fuse.conf", "/etc/fuse.conf"))
