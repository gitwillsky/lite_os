#!/usr/bin/env python3
"""固定 Mesa EGL/GBM/VirGL 运行时；只安装实际 ELF 依赖闭包，下载和上游输入留在 .local。"""
from __future__ import annotations
import json
import os
import re
import shutil
import subprocess
import tarfile
from contextlib import ExitStack
from pathlib import Path, PurePosixPath
from apk_cache import download
from build_cache import cache_lock, fingerprint, manifest_matches, publish_directory, sha256, temporary_directory, write_manifest
from build_target import target_from_environment

ROOT = Path(__file__).resolve().parent.parent
PACKAGES = Path(__file__).with_name("mesa-runtime-packages.json")
LIBRARIES = "/usr/lib/liteos"
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
    payload = {"kind": "mesa-egl-runtime", "recipe_version": 1, "arch": target.arch, "packages": packages, "seeds": SEEDS}
    cache = ROOT / "target" / "mesa-runtime" / target.arch
    final = cache / fingerprint(payload)
    with cache_lock(cache / ".lock"):
        if not manifest_matches(final, payload, ("files.json",)):
            temporary = temporary_directory(cache, "runtime")
            try:
                _extract_closure(packages, temporary)
                write_manifest(temporary, payload)
                publish_directory(temporary, final)
            finally:
                shutil.rmtree(temporary, ignore_errors=True)
    files = json.loads((final / "files.json").read_text())
    for entry in files:
        if sha256(final / entry["path"]) != entry["sha256"]:
            raise RuntimeError(f"corrupt Mesa runtime cache: {entry['path']}")
    return tuple(UserlandArtifact(final / entry["path"], "/" + entry["path"], 0o755) for entry in files)


def _extract_closure(packages, output: Path) -> None:
    from verify_artifacts import llvm_tool
    readelf = llvm_tool("llvm-readelf")
    entries = {}
    with ExitStack() as stack:
        for package in packages:
            archive = stack.enter_context(tarfile.open(download(package["name"], package["sha256"]), "r:gz"))
            for member in archive.getmembers():
                if member.name.startswith("usr/lib/") and ".so" in member.name:
                    if member.name in entries:
                        raise RuntimeError(f"duplicate upstream Mesa path: {member.name}")
                    entries[member.name] = (archive, member)
        def resolve(name):
            visited = set()
            while name not in visited:
                visited.add(name)
                archive, member = entries[name]
                if member.isfile():
                    return archive, member
                if member.issym():
                    name = os.path.normpath(str(PurePosixPath(name).parent / member.linkname))
                elif member.islnk():
                    name = member.linkname
                else:
                    break
            raise RuntimeError(f"invalid upstream Mesa library link: {name}")
        pending = dict(SEEDS)
        installed = {}
        while pending:
            destination, source = pending.popitem()
            if destination in installed:
                continue
            archive, member = resolve(source)
            path = output / destination.removeprefix("/")
            path.parent.mkdir(parents=True, exist_ok=True)
            stream = archive.extractfile(member)
            if stream is None:
                raise RuntimeError(f"Mesa library has no payload: {source}")
            with stream, path.open("wb") as target:
                shutil.copyfileobj(stream, target)
            metadata = subprocess.check_output([str(readelf), "--file-header", "--dynamic", str(path)], text=True)
            if "AArch64" not in metadata or "ELF64" not in metadata:
                raise RuntimeError(f"wrong Mesa library architecture: {source}")
            installed[destination] = path
            for dependency in re.findall(r"\(NEEDED\).*\[([^]]+)\]", metadata):
                if dependency in ("libc.musl-aarch64.so.1", "libc.so"):
                    continue
                if f"usr/lib/{dependency}" not in entries:
                    raise RuntimeError(f"Mesa dependency missing: {source} -> {dependency}")
                pending[f"{LIBRARIES}/{dependency}"] = f"usr/lib/{dependency}"
    files = [{"path": name.removeprefix("/"), "sha256": sha256(path)} for name, path in sorted(installed.items())]
    (output / "files.json").write_text(json.dumps(files, indent=2) + "\n")

if __name__ == "__main__":
    artifacts = mesa_userland()
    print(f"Mesa EGL/GBM/VirGL: {len(artifacts)} files, {sum(file.source.stat().st_size for file in artifacts) // (1024 * 1024)} MiB")
