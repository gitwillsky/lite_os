#!/usr/bin/env python3
"""把固定 Codex/Claude 工具链安装到持久 AArch64 开发镜像。"""

from __future__ import annotations

import argparse
import fcntl
import json
import subprocess
import tempfile
from pathlib import Path

from agent_cli_cache import (
    CLAUDE_VERSION,
    CODEX_VERSION,
    ROOT,
    AgentCliArtifacts,
    artifact_payload,
    cached_agent_cli_artifacts,
)
from build_cache import fingerprint, sha256
from ext2_image import (
    ensure_ext2_capacity,
    find_debugfs,
    recover_ext2_journal,
    run_debugfs,
)
from qemu_gate import boot

INSTALL_SCRIPT = ROOT / "scripts/fixtures/agent-development/install.sh"
STAMP_PATH = "/usr/share/liteos/agent-development.json"
DEFAULT_IMAGE_SIZE_MIB = 32768
DEFAULT_QEMU_MEMORY = "6G"
RECIPE_VERSION = 1
FORBIDDEN_MARKERS = (
    "unsupported syscall_id:",
    "panicked at",
    "[ERROR]",
    "Illegal instruction",
    "Segmentation fault",
)


def _run_debugfs_batch(image: Path, commands: list[str], directory: Path) -> None:
    script = directory / "agent-development.debugfs"
    script.write_text("\n".join(commands) + "\n")
    result = subprocess.run(
        [str(find_debugfs()), "-w", "-f", str(script), str(image)],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-60:])
        raise RuntimeError(f"Agent development debugfs transaction failed\n{tail}")


def installation_payload(artifacts: AgentCliArtifacts) -> dict[str, object]:
    """返回 Guest 开发环境的唯一内容身份。"""
    return {
        "kind": "agent-development-image",
        "recipe_version": RECIPE_VERSION,
        "artifacts": artifact_payload(
            artifacts.codex_binary,
            artifacts.claude_key,
            artifacts.claude_index,
            artifacts.claude_apk,
            artifacts.alpine_apks,
        ),
        "install_script_sha256": sha256(INSTALL_SCRIPT),
    }


def _read_stamp(image: Path) -> dict[str, object] | None:
    output = run_debugfs(image, f"cat {STAMP_PATH}")
    for line in reversed(output.splitlines()):
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            return value
    return None


def _try_dump(image: Path, guest_path: str, host_path: Path) -> bool:
    result = subprocess.run(
        [str(find_debugfs()), "-R", f"dump -p {guest_path} {host_path}", str(image)],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    return result.returncode == 0 and host_path.is_file()


def _dump(image: Path, guest_path: str, host_path: Path) -> None:
    if not _try_dump(image, guest_path, host_path):
        output = run_debugfs(image, f"stat {guest_path}")
        tail = "\n".join(output.splitlines()[-40:])
        raise RuntimeError(f"failed to read {guest_path} from development image\n{tail}")


def _verify_installed_image(
    image: Path,
    artifacts: AgentCliArtifacts,
    expected_identity: str,
    directory: Path,
) -> None:
    stamp = _read_stamp(image)
    if stamp is None or stamp.get("identity") != expected_identity:
        raise RuntimeError("Agent development stamp was not published")

    installed_codex = directory / "installed-codex"
    _dump(image, "/usr/local/bin/codex", installed_codex)
    if sha256(installed_codex) != sha256(artifacts.codex_binary):
        raise RuntimeError("installed Codex binary does not match fixed artifact")

    database = run_debugfs(image, "cat /lib/apk/db/installed")
    for package in ("claude-code", "bash", "curl", "git", "ripgrep"):
        if f"P:{package}\n" not in database:
            raise RuntimeError(f"Agent development APK database lacks {package}")


def _stage_installation(
    image: Path,
    artifacts: AgentCliArtifacts,
    payload: dict[str, object],
    identity: str,
    directory: Path,
) -> None:
    normal_inittab = directory / "normal.inittab"
    if not _try_dump(
        image,
        "/run/liteos-agent/normal.inittab",
        normal_inittab,
    ):
        _dump(image, "/etc/inittab", normal_inittab)
    versions = directory / "versions"
    versions.write_text(
        f"LITEOS_CODEX_VERSION='{CODEX_VERSION}'\n"
        f"LITEOS_CLAUDE_VERSION='{CLAUDE_VERSION}'\n"
    )
    stamp = directory / "stamp.json"
    stamp.write_text(
        json.dumps(
            {
                "identity": identity,
                "payload": payload,
                "recipe_version": RECIPE_VERSION,
            },
            sort_keys=True,
            separators=(",", ":"),
        )
        + "\n"
    )
    bootstrap_inittab = directory / "bootstrap.inittab"
    bootstrap_inittab.write_text(
        "::sysinit:/bin/sh /run/liteos-agent/install.sh\n"
    )

    stale_files = [
        "/run/liteos-agent/install.sh",
        "/run/liteos-agent/versions",
        "/run/liteos-agent/stamp.json",
        "/run/liteos-agent/normal.inittab",
    ]
    stale_files.extend(
        f"/run/liteos-agent/apks/{archive.name}"
        for archive in (*artifacts.alpine_apks, artifacts.claude_apk)
    )
    commands = [f"rm {path}" for path in stale_files]
    commands.extend(
        (
            "rmdir /run/liteos-agent/apks",
            "rmdir /run/liteos-agent",
        )
    )
    commands.extend(
        [
            "mkdir /run/liteos-agent",
            "mkdir /run/liteos-agent/apks",
            f"write {INSTALL_SCRIPT} /run/liteos-agent/install.sh",
            "set_inode_field /run/liteos-agent/install.sh mode 0100755",
            f"write {versions} /run/liteos-agent/versions",
            f"write {stamp} /run/liteos-agent/stamp.json",
            f"write {normal_inittab} /run/liteos-agent/normal.inittab",
            "mkdir /usr/local/bin",
            "rm /usr/local/bin/codex",
            f"write {artifacts.codex_binary} /usr/local/bin/codex",
            "set_inode_field /usr/local/bin/codex mode 0100755",
        ]
    )
    commands.extend(
        f"write {archive} /run/liteos-agent/apks/{archive.name}"
        for archive in (*artifacts.alpine_apks, artifacts.claude_apk)
    )
    commands.extend(("rm /etc/inittab", f"write {bootstrap_inittab} /etc/inittab"))
    _run_debugfs_batch(image, commands, directory)


def prepare(
    image: Path,
    size_mib: int,
    qemu_memory: str,
) -> bool:
    """安装并验证持久 Agent 开发环境。

    Args:
        image: 当前未被 QEMU 使用的 AArch64 开发镜像。
        size_mib: 开发文件系统最小容量；只增长、不缩容。
        qemu_memory: 安装/runtime smoke 使用的 QEMU RAM 参数。

    Returns:
        实际安装返回 True；内容身份已命中返回 False。

    Raises:
        RuntimeError: 镜像占用、artifact、APK transaction 或 CLI runtime 检查失败。
        OSError: 下载、镜像或 host 工具不可用。
    """
    if not image.is_file():
        raise RuntimeError(f"development image is missing: {image}")
    artifacts = cached_agent_cli_artifacts()
    payload = installation_payload(artifacts)
    identity = fingerprint(payload)

    with image.open("r+b") as stream:
        try:
            fcntl.lockf(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise RuntimeError(f"development image is already in use: {image}") from error
        recover_ext2_journal(image)
        ensure_ext2_capacity(image, size_mib)
        if (_read_stamp(image) or {}).get("identity") == identity:
            with tempfile.TemporaryDirectory(
                prefix="liteos-agent-development-check-"
            ) as temporary:
                _verify_installed_image(
                    image,
                    artifacts,
                    identity,
                    Path(temporary),
                )
            print(f"Agent development cache hit: {identity[:12]}")
            return False

        with tempfile.TemporaryDirectory(
            prefix="liteos-agent-development-"
        ) as temporary:
            directory = Path(temporary)
            _stage_installation(image, artifacts, payload, identity, directory)

    boot(
        image,
        4,
        ("LITEOS_AGENT_DEVELOPMENT_READY",),
        timeout_seconds=180,
        forbidden_markers=FORBIDDEN_MARKERS,
        persistent_writes=True,
        memory=qemu_memory,
        success_settle_seconds=1.0,
    )
    recover_ext2_journal(image)
    with tempfile.TemporaryDirectory(
        prefix="liteos-agent-development-verify-"
    ) as temporary:
        _verify_installed_image(image, artifacts, identity, Path(temporary))
    print(
        f"Agent development image prepared: Codex {CODEX_VERSION}, "
        f"Claude {CLAUDE_VERSION} ({identity[:12]})"
    )
    return True


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--image",
        type=Path,
        default=ROOT / "fs-aarch64.img",
    )
    parser.add_argument("--size-mib", type=int, default=DEFAULT_IMAGE_SIZE_MIB)
    parser.add_argument("--qemu-memory", default=DEFAULT_QEMU_MEMORY)
    args = parser.parse_args()
    prepare(args.image.resolve(), args.size_mib, args.qemu_memory)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"Agent development preparation failed: {error}")
        raise SystemExit(1)
