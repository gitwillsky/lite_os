#!/usr/bin/env python3
"""把最新系统文件离线同步到持久开发镜像。"""

from __future__ import annotations

import argparse
import fcntl
import json
import stat
import subprocess
import tempfile
from collections.abc import Sequence
from pathlib import Path, PurePosixPath

from build_cache import fingerprint, sha256
from build_target import target_from_environment
from ext4_image import find_debugfs, recover_ext4_journal
from verify_busybox import ACCOUNT_APPLETS, UserlandArtifact, base_userland, build_busybox, obtain_source
from verify_musl import cached_musl_paths, find_compiler

ROOT = Path(__file__).resolve().parent.parent
STAMP_PATH = "/usr/share/liteos/.userland-sync.json"
RECIPE_VERSION = 3


# 仅用于离线撤销旧镜像中已退休的产品文件，不参与构建或启动。
# 无历史 stamp 的旧 rootfs 也必须清理，否则旧 init 与应用仍会在开发实例里出现。
RETIRED_FILES = tuple(f"/bin/{name}" for name in (
    "audio-service", "console", "seatd", "seatctl", "desktop",
    "file-manager", "my-computer", "music-player", "terminal", "session-launch",
    "terminal-session",
)) + ("/etc/init.d/graphical-session", "/etc/terminfo/l/liteos")
RETIRED_TREES = ("/usr/lib/lite-runtime", "/usr/share/liteos")


def managed_path(path: str) -> bool:
    """只接受同步器拥有的规范绝对路径，拒绝父目录跳转。"""
    if not path.startswith("/") or str(PurePosixPath(path)) != path or ".." in PurePosixPath(path).parts:
        return False
    # Wine 运行时（--wine）：运行时树、fontconfig 与 XKB 配置由同步器拥有，缺少它旧文件撤不掉。
    if path.startswith(("/opt/wine/", "/etc/fonts/", "/usr/share/fontconfig/", "/usr/share/X11/xkb/")):
        return True
    if path.startswith("/usr/lib/liteos/") or (path.startswith("/etc/ld-musl-") and path.endswith(".path")):
        return True
    # 同步器拥有 user/base 下全部桌面条目的安装位置；缺少它，新增的条目装不进也撤不掉。
    if path.startswith("/usr/share/applications/") and path.endswith(".desktop"):
        return True
    return path in {
        *RETIRED_FILES, *(f"/bin/{name}" for name in ACCOUNT_APPLETS), "/bin/shutdown", "/bin/compositor", "/etc/inittab", "/etc/init.d/rcS",
        "/etc/init.d/display-service", "/etc/profile", "/bin/logon", "/bin/desktop", "/bin/file-explorer", "/bin/control-panel", "/bin/taskmgr", "/bin/sndvol", "/bin/sndplay", "/bin/mplay", "/bin/notepad", "/bin/devmgmt", "/bin/session-launch",
    } or any(path.startswith(root + "/") for root in RETIRED_TREES)


def retired_commands(image: Path, retained: set[str], directory: Path) -> list[str]:
    """按子文件先于目录的顺序删除旧产品文件，不跟随符号链接。

    Args:
        image: 已取得独占锁的开发镜像。
        retained: 当前系统清单仍拥有的文件。
        directory: debugfs batch 的临时目录。

    Returns:
        限定在旧产品路径内的删除命令；不触碰 /root 或 APK 安装的其他文件。

    Raises:
        RuntimeError: debugfs 执行失败或目录项无法安全解析。
    """
    commands = [f"rm {path}" for path in RETIRED_FILES if path not in retained]

    def visit(path: str) -> None:
        listing = run_batch(image, [f"ls -p {path}"], directory, "list-retired", writable=False)
        for line in listing.splitlines():
            if not line.startswith("/") or ": File not found" in line:
                continue
            fields = line.split("/")
            if len(fields) < 7:
                raise RuntimeError(f"invalid debugfs directory entry: {line}")
            name = fields[5]
            if name in (".", "..", "") or fields[1] == "0":
                continue
            child = f"{path}/{name}"
            if not managed_path(child):
                raise RuntimeError(f"retired file escaped managed paths: {child}")
            if stat.S_ISDIR(int(fields[2], 8)):
                visit(child)
                if not any(item.startswith(child + "/") for item in retained):
                    commands.append(f"rmdir {child}")
            elif child not in retained and child != STAMP_PATH:
                commands.append(f"rm {child}")

    for path in RETIRED_TREES:
        visit(path)
        if not any(item.startswith(path + "/") for item in retained):
            commands.append(f"rmdir {path}")
    return commands


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
        "kind": "base-userland-sync",
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
        artifacts: 当前构建得到的完整系统文件集合。

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
        recover_ext4_journal(image)
        # 仅迁移完整匹配旧出厂模板的锁定默认；任何实际密码、自定义锁定或新增账户都必须保留。
        account_changed = False
        with tempfile.TemporaryDirectory(prefix="liteos-account-bootstrap-") as temporary:
            directory = Path(temporary)
            metadata = run_batch(image, ["stat /etc/shadow"], directory, "inspect-shadow", writable=False)
            missing = "Inode:" not in metadata
            previous_default = False
            if not missing:
                saved = directory / "shadow"
                run_batch(image, [f"dump -p /etc/shadow {saved}"], directory, "read-shadow", writable=False)
                previous_default = saved.is_file() and saved.read_bytes() == b"root:!:1:0:99999:7:::\nnobody:!:1:0:99999:7:::\n"
            if missing or previous_default:
                commands = [] if missing else ["rm /etc/shadow"]
                commands += [f"write {ROOT / 'user/base/shadow'} /etc/shadow", "set_inode_field /etc/shadow mode 0100600"]
                run_batch(image, commands, directory, "initialize-shadow", writable=True)
                account_changed = True
        # /etc/localtime 是用户拥有的时区设置（控制面板会改写它）：只在缺失时写入默认值，绝不覆盖。
        with tempfile.TemporaryDirectory(prefix="liteos-localtime-") as temporary:
            directory = Path(temporary)
            metadata = run_batch(image, ["stat /etc/localtime"], directory, "inspect-localtime", writable=False)
            if "Inode:" not in metadata:
                run_batch(image, [f"write {ROOT / 'user/base/localtime'} /etc/localtime"], directory, "initialize-localtime", writable=True)
        previous = read_stamp(image)
        if previous is not None and previous.get("identity") == identity:
            print(f"userland sync cache hit: {identity[:12]}")
            return account_changed

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
            retired = retired_commands(image, current_paths, directory)
            commands = [f"rm {STAMP_PATH}", *retired]
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
            removed_paths = [command.split(" ", 1)[1] for command in retired]
            removed_paths.extend(stale_paths)
            retired_metadata = run_batch(
                image, [f"stat {path}" for path in removed_paths], directory,
                "verify-retirement", writable=False,
            )
            if "Inode:" in retired_metadata:
                raise RuntimeError("retired userland files remain; sync stamp was not published")

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
    """将当前目标的系统文件同步到指定开发镜像。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument(
        "--wine",
        action="store_true",
        help="同时安装 Wine 运行时（/opt/wine 约 770 MiB，会先把镜像扩到足够容量）",
    )
    arguments = parser.parse_args()
    image = arguments.image.resolve()
    if not image.is_file():
        raise RuntimeError(f"development image does not exist: {image}")

    compiler = find_compiler()
    binary = build_busybox(obtain_source(), compiler, None)
    artifacts = base_userland(cached_musl_paths(compiler))
    # 同一个固定 BusyBox ELF 以标准 applet 名安装，旧开发镜像无需替换 init 或覆盖账户数据。
    artifacts += tuple(UserlandArtifact(binary, f"/bin/{name}", 0o755) for name in ACCOUNT_APPLETS)
    if arguments.wine:
        # 同一份清单里要么有 Wine 要么没有：不带 --wine 的同步会按指纹把已安装的 Wine 撤掉，
        # 所以开发镜像始终用同一个选择同步。
        from ext4_image import ensure_ext4_capacity
        from wine_runtime import REQUIRED_CAPACITY_MIB, wine_userland

        ensure_ext4_capacity(image, REQUIRED_CAPACITY_MIB)
        artifacts += wine_userland()
    synchronize(image, artifacts)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
