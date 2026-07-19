#!/usr/bin/env python3
"""为构建 gate 提供内容寻址 manifest、锁和原子发布原语。"""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import shutil
import subprocess
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator

from qemu_gate import qemu_runtime

CACHE_MANIFEST = ".liteos-cache.json"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _canonical_recipe(
    payload: dict[str, object],
) -> tuple[bytes, dict[str, object]]:
    """返回 fingerprint 与持久 manifest 共用的规范 JSON recipe。"""
    encoded = json.dumps(
        payload,
        allow_nan=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return encoded, json.loads(encoded)


def fingerprint(payload: dict[str, object]) -> str:
    encoded, _ = _canonical_recipe(payload)
    return hashlib.sha256(encoded).hexdigest()


def expected_manifest(payload: dict[str, object]) -> dict[str, object]:
    encoded, recipe = _canonical_recipe(payload)
    return {"fingerprint": hashlib.sha256(encoded).hexdigest(), "recipe": recipe}


def manifest_matches(
    directory: Path,
    payload: dict[str, object],
    required_files: tuple[str, ...],
) -> bool:
    try:
        manifest = json.loads((directory / CACHE_MANIFEST).read_text())
    except (OSError, json.JSONDecodeError):
        return False
    return manifest == expected_manifest(payload) and all(
        (directory / relative).is_file() for relative in required_files
    )


def write_manifest(directory: Path, payload: dict[str, object]) -> None:
    path = directory / CACHE_MANIFEST
    temporary = directory / f".{CACHE_MANIFEST}.{os.getpid()}.tmp"
    temporary.write_text(json.dumps(expected_manifest(payload), sort_keys=True) + "\n")
    os.replace(temporary, path)


@contextmanager
def cache_lock(path: Path) -> Iterator[None]:
    """串行化同一 cache 的 writer，避免并发进程观察半成品。"""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(lock.fileno(), fcntl.LOCK_UN)


def temporary_directory(parent: Path, label: str) -> Path:
    parent.mkdir(parents=True, exist_ok=True)
    path = parent / f".{label}.{os.getpid()}.{time.time_ns()}.tmp"
    path.mkdir()
    return path


def generation_directory(parent: Path, fingerprint_value: str) -> Path:
    parent.mkdir(parents=True, exist_ok=True)
    path = parent / f"{fingerprint_value}-{os.getpid()}-{time.time_ns()}"
    path.mkdir()
    return path


def publish_directory(temporary: Path, final: Path) -> None:
    """只在完整产物和 manifest 就绪后发布 content-addressed directory。"""
    final.parent.mkdir(parents=True, exist_ok=True)
    if final.exists():
        shutil.rmtree(final)
    temporary.rename(final)


def publish_generation(generation: Path, link: Path) -> None:
    """原子切换 fingerprint symlink；已打开的旧 generation 保持有效。"""
    link.parent.mkdir(parents=True, exist_ok=True)
    # 1. 临时链接只指向已经写完 manifest 的不可变 generation。
    temporary_link = link.parent / f".{link.name}.{os.getpid()}.{time.time_ns()}.tmp"
    temporary_link.symlink_to(generation.resolve(), target_is_directory=True)
    # 2. 旧版 cache 可能在链接位置留下普通目录；不隔离它会使原子 replace 直接失败。
    if link.is_dir() and not link.is_symlink():
        quarantine = link.parent / f".{link.name}.invalid.{time.time_ns()}"
        link.rename(quarantine)
        shutil.rmtree(quarantine)
    # 3. 最后一步才切换公开入口；异常时删除未发布的临时链接。
    try:
        os.replace(temporary_link, link)
    finally:
        temporary_link.unlink(missing_ok=True)


def build_jobs_override() -> int | None:
    override = os.environ.get("LITEOS_BUILD_JOBS")
    if override is None:
        return None
    try:
        jobs = int(override)
    except ValueError as error:
        raise RuntimeError("LITEOS_BUILD_JOBS must be a positive integer") from error
    if jobs <= 0:
        raise RuntimeError("LITEOS_BUILD_JOBS must be a positive integer")
    return jobs


def make_command(jobs_override: int | None) -> list[str]:
    if jobs_override is not None:
        return ["make", f"-j{jobs_override}"]
    makeflags = os.environ.get("MAKEFLAGS", "")
    if "--jobserver-auth=" in makeflags or "--jobserver-fds=" in makeflags:
        return ["make"]
    return ["make", f"-j{os.cpu_count() or 1}"]


def build_environment() -> dict[str, str]:
    env = os.environ.copy()
    env["LC_ALL"] = "C"
    for name in ("CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "LIBRARY_PATH"):
        env.pop(name, None)
    return env


def runtime_gate_payload(
    kind: str,
    recipe_version: int,
    inputs: tuple[Path, ...],
) -> dict[str, object]:
    """构造只由实际执行比特、gate recipe 与 QEMU identity 决定的成功缓存键。"""
    runtime = qemu_runtime()
    qemu = shutil.which(runtime.binary)
    if qemu is None:
        raise RuntimeError(f"{runtime.binary} is required")
    version = subprocess.run(
        [qemu, "--version"],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    ).stdout.splitlines()[0]
    return {
        "kind": kind,
        "recipe_version": recipe_version,
        "inputs": {str(path): sha256(path) for path in inputs},
        "qemu": {
            "path": str(Path(qemu).resolve()),
            "version": version,
            "arch": runtime.arch,
            "accel": runtime.acceleration,
            "cpu": runtime.cpu,
            "machine": runtime.machine,
            "kernel_boot_artifact": runtime.kernel_boot_artifact,
        },
    }


def runtime_gate_hit(
    stamp: Path,
    payload: dict[str, object],
    required_artifacts: tuple[Path, ...] = (),
) -> bool:
    """仅当 payload 一致且当前 invocation 所需产物仍存在时命中。"""
    if os.environ.get("LITEOS_VERIFY_REBUILD") == "1":
        return False
    if any(not artifact.is_file() for artifact in required_artifacts):
        return False
    try:
        current = json.loads(stamp.read_text())
    except (OSError, json.JSONDecodeError):
        return False
    return current == expected_manifest(payload)


def publish_runtime_gate(stamp: Path, payload: dict[str, object]) -> None:
    """在全部 runtime assertions 成功后原子发布 success stamp。"""
    stamp.parent.mkdir(parents=True, exist_ok=True)
    temporary = stamp.parent / f".{stamp.name}.{os.getpid()}.{time.time_ns()}.tmp"
    temporary.write_text(json.dumps(expected_manifest(payload), sort_keys=True) + "\n")
    os.replace(temporary, stamp)
