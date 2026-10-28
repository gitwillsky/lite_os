#!/usr/bin/env python3
"""从固定 Alpine APK 取出共享库及其真实 ELF DT_NEEDED 闭包；Mesa 与 mpv 运行时共用。"""
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
from build_cache import sha256

LIBRARIES = "/usr/lib/liteos"
# DT_NEEDED 只给 soname；上游把少数私有库放在子目录（libpulse 的 RPATH 指向 usr/lib/pulseaudio）。
SEARCH_DIRECTORIES = ("usr/lib/", "usr/lib/pulseaudio/")
LIBRARY_MODE = 0o755
DATA_MODE = 0o644


def extract_closure(packages, seeds: dict[str, str], output: Path, *, label: str, machine: str,
                    data: dict[str, str] | None = None, trees: dict[str, str] | None = None, provided: frozenset[str] = frozenset()) -> None:
    """把 `seeds` 及其依赖闭包写入 `output`，并生成 `files.json`（路径、SHA-256、文件模式）。

    Args:
        packages: `[{"name", "sha256"}]`，APK 由固定摘要裁决。
        seeds: `{安装路径: APK 内共享库路径}`，符号链接解析到真实文件后以安装路径落盘。
        output: 空的临时目录。
        label: 错误信息中的运行时名称。
        machine: `llvm-readelf` 报告的机器名（如 `AArch64`），不符即失败。
        data: `{安装路径: APK 内普通文件}`，不是 ELF 的配置/数据文件。
        trees: `{安装目录: APK 内目录}`，目录下的全部普通文件原样安装（如 alsa-lib 的 `usr/share/alsa`）。
        provided: 其他运行时已安装的 `/usr/lib/liteos/<soname>`；依赖落在其中时不再安装，也不再向下遍历。

    Raises:
        RuntimeError: 路径重复、链接无效、ELF 架构不符或某个 DT_NEEDED 没有任何 APK 提供。
    """
    from verify_artifacts import llvm_tool
    readelf = llvm_tool("llvm-readelf")
    data = dict(data or {})
    trees = trees or {}
    wanted = set(data.values())
    entries = {}
    with ExitStack() as stack:
        for package in packages:
            archive = stack.enter_context(tarfile.open(download(package["name"], package["sha256"]), "r:gz"))
            for member in archive.getmembers():
                for directory, source in trees.items():
                    if member.isfile() and member.name.startswith(source.rstrip("/") + "/"):
                        data[directory.rstrip("/") + member.name[len(source.rstrip("/")):]] = member.name
                        wanted.add(member.name)
                if (member.name.startswith("usr/lib/") and ".so" in member.name) or member.name in wanted:
                    if member.name in entries:
                        raise RuntimeError(f"duplicate upstream {label} path: {member.name}")
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
            raise RuntimeError(f"invalid upstream {label} library link: {name}")

        def write(destination, source):
            archive, member = resolve(source)
            path = output / destination.removeprefix("/")
            path.parent.mkdir(parents=True, exist_ok=True)
            stream = archive.extractfile(member)
            if stream is None:
                raise RuntimeError(f"{label} file has no payload: {source}")
            with stream, path.open("wb") as target:
                shutil.copyfileobj(stream, target)
            return path

        pending = dict(seeds)
        installed = {}
        while pending:
            destination, source = pending.popitem()
            if destination in installed:
                continue
            path = write(destination, source)
            metadata = subprocess.check_output([str(readelf), "--file-header", "--dynamic", str(path)], text=True)
            if machine not in metadata or "ELF64" not in metadata:
                raise RuntimeError(f"wrong {label} library architecture: {source}")
            installed[destination] = path
            for dependency in re.findall(r"\(NEEDED\).*\[([^]]+)\]", metadata):
                if re.fullmatch(r"libc\.musl-[a-z0-9_]+\.so\.1|libc\.so", dependency):
                    continue
                target = f"{LIBRARIES}/{dependency}"
                if target in provided:
                    continue
                found = next((f"{directory}{dependency}" for directory in SEARCH_DIRECTORIES
                              if f"{directory}{dependency}" in entries), None)
                if found is None:
                    raise RuntimeError(f"{label} dependency missing: {source} -> {dependency}")
                pending[target] = found
        files = [{"path": name.removeprefix("/"), "sha256": sha256(path), "mode": LIBRARY_MODE}
                 for name, path in sorted(installed.items())]
        for destination, source in sorted(data.items()):
            path = write(destination, source)
            files.append({"path": destination.removeprefix("/"), "sha256": sha256(path), "mode": DATA_MODE})
    (output / "files.json").write_text(json.dumps(files, indent=2) + "\n")
