#!/usr/bin/env python3
"""固定 libmpv/FFmpeg 运行时；播放器经 dlopen 使用上游库，只安装实际 ELF 依赖闭包。"""
from __future__ import annotations
import json
import shutil
from pathlib import Path
from apk_elf_closure import LIBRARIES, extract_closure
from build_cache import cache_lock, fingerprint, manifest_matches, publish_directory, sha256, temporary_directory, write_manifest
from build_target import target_from_environment
from mesa_runtime import mesa_userland

ROOT = Path(__file__).resolve().parent.parent
PACKAGES = Path(__file__).with_name("mpv-runtime-packages.json")
SEEDS = {f"{LIBRARIES}/libmpv.so.2": "usr/lib/libmpv.so.2"}
# alsa-lib 在编译期固定从 /usr/share/alsa 读取 alsa.conf 与 cards/pcm 配置；缺失时 snd_pcm_open 直接失败。
TREES = {"/usr/share/alsa": "usr/share/alsa"}


def mpv_userland():
    """返回 AArch64 libmpv 的受控安装清单；其他 target 不安装，播放器在运行期报告缺少 libmpv。

    Mesa 运行时已经安装的 soname 不再重复安装。任一 APK 哈希、ELF 架构或 DT_NEEDED 闭包不完整即失败。
    """
    from userland import UserlandArtifact
    target = target_from_environment()
    if target.arch != "aarch64":
        return ()
    provided = frozenset(item.destination for item in mesa_userland())
    packages = json.loads(PACKAGES.read_text())
    payload = {"kind": "mpv-runtime", "recipe_version": 1, "arch": target.arch, "packages": packages,
               "seeds": SEEDS, "trees": TREES, "provided": sorted(provided)}
    cache = ROOT / "target" / "mpv-runtime" / target.arch
    final = cache / fingerprint(payload)
    with cache_lock(cache / ".lock"):
        if not manifest_matches(final, payload, ("files.json",)):
            temporary = temporary_directory(cache, "runtime")
            try:
                extract_closure(packages, SEEDS, temporary, label="mpv", machine="AArch64", trees=TREES, provided=provided)
                write_manifest(temporary, payload)
                publish_directory(temporary, final)
            finally:
                shutil.rmtree(temporary, ignore_errors=True)
    files = json.loads((final / "files.json").read_text())
    for entry in files:
        if sha256(final / entry["path"]) != entry["sha256"]:
            raise RuntimeError(f"corrupt mpv runtime cache: {entry['path']}")
    return tuple(UserlandArtifact(final / entry["path"], "/" + entry["path"], entry["mode"]) for entry in files)


if __name__ == "__main__":
    artifacts = mpv_userland()
    print(f"libmpv/FFmpeg: {len(artifacts)} files, {sum(file.source.stat().st_size for file in artifacts) // (1024 * 1024)} MiB")
