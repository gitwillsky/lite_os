#!/usr/bin/env python3
"""固定 Mesa EGL/GBM/VirGL 运行时；只安装实际 ELF 依赖闭包，下载和上游输入留在 .local。"""
from __future__ import annotations
import json
import shutil
from pathlib import Path
from apk_elf_closure import LIBRARIES, extract_closure
from build_cache import cache_lock, fingerprint, manifest_matches, publish_directory, sha256, temporary_directory, write_manifest
from build_target import target_from_environment

ROOT = Path(__file__).resolve().parent.parent
PACKAGES = Path(__file__).with_name("mesa-runtime-packages.json")
# loader 的标准运行时位置；不安装其他 GPU 的 driver 或悄悄切换到软件驱动。
SEEDS = {
    f"{LIBRARIES}/libEGL.so.1": "usr/lib/libEGL.so.1",
    f"{LIBRARIES}/libGLESv2.so.2": "usr/lib/libGLESv2.so.2",
    f"{LIBRARIES}/libGLESv1_CM.so.1": "usr/lib/libGLESv1_CM.so.1",
    f"{LIBRARIES}/libgbm.so.1": "usr/lib/libgbm.so.1",
    f"{LIBRARIES}/libwayland-egl.so.1": "usr/lib/libwayland-egl.so.1",
    "/usr/lib/gbm/dri_gbm.so": "usr/lib/gbm/dri_gbm.so",
    "/usr/lib/dri/virtio_gpu_dri.so": "usr/lib/dri/virtio_gpu_dri.so",
}

def mesa_userland():
    """返回 AArch64 Mesa 25.1.9 的受控安装清单；其他 target 不安装不存在的 VirGL 产品运行时。

    任一 APK 哈希、ELF 架构或 DT_NEEDED 闭包不完整即失败，绝不覆盖系统 musl。
    """
    from verify_busybox import UserlandArtifact
    target = target_from_environment()
    if target.arch != "aarch64":
        return ()
    packages = json.loads(PACKAGES.read_text())
    payload = {"kind": "mesa-egl-runtime", "recipe_version": 2, "arch": target.arch, "packages": packages, "seeds": SEEDS}
    cache = ROOT / "target" / "mesa-runtime" / target.arch
    final = cache / fingerprint(payload)
    with cache_lock(cache / ".lock"):
        if not manifest_matches(final, payload, ("files.json",)):
            temporary = temporary_directory(cache, "runtime")
            try:
                extract_closure(packages, SEEDS, temporary, label="Mesa", machine="AArch64")
                write_manifest(temporary, payload)
                publish_directory(temporary, final)
            finally:
                shutil.rmtree(temporary, ignore_errors=True)
    files = json.loads((final / "files.json").read_text())
    for entry in files:
        if sha256(final / entry["path"]) != entry["sha256"]:
            raise RuntimeError(f"corrupt Mesa runtime cache: {entry['path']}")
    return tuple(UserlandArtifact(final / entry["path"], "/" + entry["path"], entry["mode"]) for entry in files)


if __name__ == "__main__":
    artifacts = mesa_userland()
    print(f"Mesa EGL/GBM/VirGL: {len(artifacts)} files, {sum(file.source.stat().st_size for file in artifacts) // (1024 * 1024)} MiB")
