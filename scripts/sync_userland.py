#!/usr/bin/env python3
"""把最新图形用户态产物离线同步到持久开发镜像。"""

from __future__ import annotations

import argparse
import fcntl
import json
import stat
import subprocess
import tempfile
from collections.abc import Sequence
from pathlib import Path, PurePosixPath

from build_cache import cache_lock, fingerprint, sha256
from build_target import target_from_environment
from ext2_image import find_debugfs, recover_ext2_journal
from verify_busybox import WORK, UserlandArtifact, build_graphical_userland
from verify_musl import cached_musl_paths, find_compiler

ROOT = Path(__file__).resolve().parent.parent
STAMP_PATH = "/usr/share/liteos/.userland-sync.json"
RECIPE_VERSION = 1


def managed_path(path: str) -> bool:
    """返回 path 是否属于增量同步唯一拥有的 guest 文件集合。"""
    return path in {
        "/bin/audio-service",
        "/bin/compositor",
        "/bin/lite-ui",
        "/bin/session-launch",
        "/bin/shutdown",
        "/bin/terminal-session",
        "/etc/inittab",
        "/etc/init.d/graphical-session",
        "/etc/profile",
        "/etc/terminfo/l/liteos",
    } or path.startswith(("/usr/lib/lite-ui/", "/usr/share/liteos/"))


def read_stamp(image: Path) -> dict[str, object] | None:
    """读取开发镜像内的同步指纹；缺失或损坏时返回 None 并执行完整修复。"""
    result = subprocess.run(
        [str(find_debugfs()), "-R", f"cat {STAMP_PATH}", str(image)],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    for line in reversed(result.stdout.splitlines()):
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            return value
    return None


def run_batch(
    image: Path,
    commands: Sequence[str],
    directory: Path,
    name: str,
    *,
    writable: bool,
) -> str:
    """执行一个 debugfs batch，并在 host 工具失败时保留可诊断尾部。

    Args:
        image: 当前已取得独占锁的离线开发镜像。
        commands: 按顺序执行的 debugfs 命令。
        directory: batch 文件所在的临时目录。
        name: 诊断中使用的 batch 名称。
        writable: 是否允许 debugfs 修改镜像。

    Returns:
        debugfs 合并后的标准输出。

    Raises:
        RuntimeError: debugfs 返回失败状态。
    """
    script = directory / f"{name}.debugfs"
    script.write_text("\n".join(commands) + "\n")
    command = [str(find_debugfs())]
    if writable:
        command.append("-w")
    command.extend(("-f", str(script), str(image)))
    result = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-60:])
        raise RuntimeError(f"{name} debugfs batch failed\n{tail}")
    return result.stdout


def artifact_payload(
    artifacts: tuple[UserlandArtifact, ...],
) -> tuple[dict[str, object], str]:
    """构造与目标架构和全部安装 bytes 绑定的同步身份。"""
    target = target_from_environment()
    files = [
        {
            "path": artifact.destination,
            "mode": artifact.mode,
            "sha256": sha256(artifact.source),
        }
        for artifact in artifacts
    ]
    payload: dict[str, object] = {
        "kind": "graphical-userland-sync",
        "recipe_version": RECIPE_VERSION,
        "arch": target.arch,
        "files": files,
    }
    return payload, fingerprint(payload)


def parent_directories(artifacts: tuple[UserlandArtifact, ...]) -> list[str]:
    """返回需要存在的 managed parent directory，父级始终排在子级之前。"""
    directories: set[str] = set()
    for artifact in artifacts:
        parent = PurePosixPath(artifact.destination).parent
        while str(parent) not in ("/", "/bin", "/etc", "/usr"):
            directories.add(str(parent))
            parent = parent.parent
    return sorted(directories, key=lambda path: (path.count("/"), path))


def verify_artifacts(
    image: Path,
    artifacts: tuple[UserlandArtifact, ...],
    directory: Path,
) -> None:
    """从镜像回读所有文件并校验 bytes 与 executable mode。"""
    output = directory / "readback"
    output.mkdir()
    commands = []
    for index, artifact in enumerate(artifacts):
        commands.append(f"dump -p {artifact.destination} {output / str(index)}")
    run_batch(image, commands, directory, "verify-userland", writable=False)
    for index, artifact in enumerate(artifacts):
        observed = output / str(index)
        if not observed.is_file() or sha256(observed) != sha256(artifact.source):
            raise RuntimeError(f"userland sync verification failed: {artifact.destination}")
        if stat.S_IMODE(observed.stat().st_mode) != artifact.mode:
            raise RuntimeError(f"userland mode verification failed: {artifact.destination}")


def synchronize(image: Path, artifacts: tuple[UserlandArtifact, ...]) -> bool:
    """同步产物并发布镜像内指纹；无变化返回 False。

    Args:
        image: 持久开发 ext 镜像，调用期间不得由 QEMU 使用。
        artifacts: 当前构建得到的完整图形用户态文件集合。

    Returns:
        实际修改镜像返回 True，已命中内容指纹返回 False。

    Raises:
        RuntimeError: 镜像正在使用、journal 恢复或任一文件写入/回读失败。
    """
    for artifact in artifacts:
        if not managed_path(artifact.destination):
            raise RuntimeError(f"userland artifact escaped managed paths: {artifact.destination}")
    payload, identity = artifact_payload(artifacts)
    with image.open("r+b") as stream:
        try:
            fcntl.lockf(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise RuntimeError(f"development image is already in use: {image}") from error
        recover_ext2_journal(image)
        previous = read_stamp(image)
        if previous is not None and previous.get("identity") == identity:
            print(f"userland sync cache hit: {identity[:12]}")
            return False

        current_paths = {artifact.destination for artifact in artifacts}
        stored_paths = previous.get("paths") if previous is not None else None
        previous_paths = stored_paths if isinstance(stored_paths, list) else []
        stale_paths = sorted(
            path
            for path in previous_paths
            if isinstance(path, str) and managed_path(path) and path not in current_paths
        )
        with tempfile.TemporaryDirectory(prefix="liteos-userland-sync-") as temporary:
            directory = Path(temporary)
            commands = [f"rm {STAMP_PATH}"]
            commands.extend(f"mkdir {path}" for path in parent_directories(artifacts))
            commands.extend(f"rm {path}" for path in stale_paths)
            for artifact in artifacts:
                commands.extend(
                    (
                        f"rm {artifact.destination}",
                        f"write {artifact.source} {artifact.destination}",
                        f"set_inode_field {artifact.destination} mode 0100{artifact.mode:o}",
                    )
                )
            run_batch(image, commands, directory, "write-userland", writable=True)
            verify_artifacts(image, artifacts, directory)

            stamp = {
                "identity": identity,
                "paths": sorted(current_paths),
                "recipe_version": RECIPE_VERSION,
            }
            stamp_source = directory / "stamp.json"
            stamp_source.write_text(
                json.dumps(stamp, sort_keys=True, separators=(",", ":")) + "\n"
            )
            run_batch(
                image,
                (
                    f"rm {STAMP_PATH}",
                    f"write {stamp_source} {STAMP_PATH}",
                    f"set_inode_field {STAMP_PATH} mode 0100644",
                ),
                directory,
                "publish-userland-stamp",
                writable=True,
            )
    print(f"userland synchronized: {len(artifacts)} files ({identity[:12]})")
    return True


def main() -> int:
    """构建当前目标的图形用户态并同步到指定开发镜像。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, required=True)
    arguments = parser.parse_args()
    image = arguments.image.resolve()
    if not image.is_file():
        raise RuntimeError(f"development image does not exist: {image}")

    compiler = find_compiler()
    with cache_lock(WORK / ".build.lock"):
        musl = cached_musl_paths(compiler)
        artifacts = build_graphical_userland(musl)
    synchronize(image, artifacts)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
