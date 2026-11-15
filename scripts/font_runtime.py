#!/usr/bin/env python3
"""发布所有图形进程共享的 Pango/Fontconfig/Cairo ELF 闭包。"""
from __future__ import annotations

import json
import shutil
from pathlib import Path

from apk_elf_closure import LIBRARIES, extract_closure
from build_cache import (
    cache_lock, fingerprint, manifest_matches, publish_directory, sha256,
    temporary_directory, write_manifest,
)
from build_target import target_from_environment
from mesa_runtime import mesa_userland
from toolchain.musl import ELF_MACHINE

ROOT = Path(__file__).resolve().parent.parent
PACKAGES = Path(__file__).with_name("font-runtime-packages.json")
SEEDS = {
    f"{LIBRARIES}/{name}": f"usr/lib/{name}"
    for name in ("libpangocairo-1.0.so.0", "libfontconfig.so.1")
}


def font_userland():
    """按目标架构解析固定包，发布一个共享字体运行库集合及唯一匹配配置。"""
    from userland import UserlandArtifact
    target = target_from_environment()
    packages = json.loads(PACKAGES.read_text())[target.arch]
    provided = frozenset(item.destination for item in mesa_userland())
    payload = {
        "kind": "font-runtime", "recipe_version": 1, "arch": target.arch,
        "packages": packages, "seeds": SEEDS, "provided": sorted(provided),
    }
    cache = ROOT / "target/font-runtime" / target.arch
    final = cache / fingerprint(payload)
    with cache_lock(cache / ".lock"):
        if not manifest_matches(final, payload, ("files.json",)):
            temporary = temporary_directory(cache, "runtime")
            try:
                extract_closure(
                    packages, SEEDS, temporary, label="font-runtime",
                    machine=ELF_MACHINE, provided=provided,
                )
                write_manifest(temporary, payload)
                publish_directory(temporary, final)
            finally:
                shutil.rmtree(temporary, ignore_errors=True)
    files = json.loads((final / "files.json").read_text())
    for entry in files:
        if sha256(final / entry["path"]) != entry["sha256"]:
            raise RuntimeError(f"corrupt font runtime cache: {entry['path']}")
    return tuple(
        UserlandArtifact(final / entry["path"], "/" + entry["path"], entry["mode"])
        for entry in files
    )
