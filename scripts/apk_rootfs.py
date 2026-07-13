#!/usr/bin/env python3
"""让签名 liteos-base package 成为最终 rootfs 文件与 package database 的唯一 owner。"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

from apk_cache import ALPINE_BRANCH, cached_apk_bootstrap
from apk_package import ApkPackageMetadata, build_signed_apk, tamper_signed_apk_control
from qemu_gate import boot

ROOT = Path(__file__).resolve().parent.parent
BASE_PACKAGE_NAME = "liteos-base"
BASE_PACKAGE_VERSION = "0.1.0-r0"
BASE_PACKAGE_FILENAME = f"{BASE_PACKAGE_NAME}-{BASE_PACKAGE_VERSION}.apk"
PROJECT_URL = "https://github.com/gitwillsky/lite_os"


def run(command: list[str]) -> str:
    """执行 rootfs assembly command，并保留失败输出。"""
    result = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-80:])
        raise RuntimeError(f"command failed: {' '.join(command)}\n{tail}")
    return result.stdout


def _inject_bootstrap_files(image: Path, debugfs: Path, workspace: Path) -> tuple[Path, Path]:
    """把 apk.static、trust roots 和 repository policy 加入 package staging 前的镜像。"""
    bootstrap = cached_apk_bootstrap()
    repositories = workspace / "repositories"
    repositories.write_text(
        f"https://dl-cdn.alpinelinux.org/alpine/{ALPINE_BRANCH}/main\n"
    )
    commands = [
        "mkdir /sbin",
        "mkdir /etc/apk",
        "mkdir /etc/apk/keys",
        f"write {bootstrap.apk_static} /sbin/apk.static",
        "set_inode_field /sbin/apk.static mode 0100755",
        "ln /sbin/apk.static /sbin/apk",
        "set_inode_field /sbin/apk.static links_count 2",
        f"write {repositories} /etc/apk/repositories",
    ]
    for key in sorted(bootstrap.alpine_keys.iterdir()):
        if key.is_file():
            commands.append(f"write {key} /etc/apk/keys/{key.name}")
    commands.append(
        f"write {bootstrap.public_key} /etc/apk/keys/{bootstrap.public_key.name}"
    )
    script = workspace / "inject-apk.debugfs"
    script.write_text("\n".join(commands) + "\n")
    run([str(debugfs), "-w", "-f", str(script), str(image)])
    return bootstrap.private_key, bootstrap.public_key


def _stage_package_root(
    image: Path,
    debugfs: Path,
    workspace: Path,
    busybox_links: tuple[str, ...],
) -> Path:
    """从唯一 ext2 image primitive 导出 package payload，并恢复 hardlink identity。"""
    staging = workspace / "rootfs"
    staging.mkdir()
    run([str(debugfs), "-R", f"rdump / {staging}", str(image)])
    for unmanaged in ("lost+found", "dev", "proc"):
        path = staging / unmanaged
        if path.is_dir():
            shutil.rmtree(path)
        else:
            path.unlink(missing_ok=True)

    init = staging / "bin/init"
    if not init.is_file():
        raise RuntimeError("rootfs staging lacks /bin/init")
    for name in busybox_links:
        path = staging / "bin" / name
        path.unlink(missing_ok=True)
        os.link(init, path)
    apk_static = staging / "sbin/apk.static"
    apk = staging / "sbin/apk"
    apk.unlink(missing_ok=True)
    os.link(apk_static, apk)
    return staging


def _build_base_package(
    staging: Path,
    workspace: Path,
    private_key: Path,
    public_key: Path,
) -> Path:
    """将当前完整 userspace 制作为唯一 signed base package。"""
    package = workspace / BASE_PACKAGE_FILENAME
    return build_signed_apk(
        staging,
        package,
        ApkPackageMetadata(
            name=BASE_PACKAGE_NAME,
            version=BASE_PACKAGE_VERSION,
            description="LiteOS fixed musl BusyBox userspace",
            url=PROJECT_URL,
            license="MIT AND GPL-2.0-only AND Apache-2.0",
            arch="riscv64",
            provides=(
                "/bin/sh",
                "cmd:apk",
                "cmd:busybox",
                "so:libc.musl-riscv64.so.1=1.2.6",
            ),
        ),
        private_key,
        public_key,
    )


def _build_fixture_package(
    workspace: Path,
    private_key: Path,
    public_key: Path,
    name: str,
    version: str,
    relative_path: str,
    contents: str | bytes,
    dependencies: tuple[str, ...] = (),
) -> Path:
    """构造不进入最终镜像的 signed package-management fixture。"""
    root = workspace / f"fixture-{name}-{version}"
    path = root / relative_path
    path.parent.mkdir(parents=True)
    if isinstance(contents, str):
        path.write_text(contents)
    else:
        path.write_bytes(contents)
    return build_signed_apk(
        root,
        workspace / f"{name}-{version}.apk",
        ApkPackageMetadata(
            name=name,
            version=version,
            description=f"LiteOS APK gate fixture {name}",
            url=PROJECT_URL,
            license="MIT",
            arch="riscv64",
            dependencies=dependencies,
        ),
        private_key,
        public_key,
    )


def _build_package_fixtures(
    workspace: Path,
    private_key: Path,
    public_key: Path,
) -> tuple[Path, ...]:
    """生成 dependency/add/del/upgrade/concurrency/signature gate 的完整输入。"""
    dependency = _build_fixture_package(
        workspace,
        private_key,
        public_key,
        "liteos-apk-dependency",
        "1.0.0-r0",
        "usr/share/liteos-apk/dependency",
        "dependency\n",
    )
    probe_v1 = _build_fixture_package(
        workspace,
        private_key,
        public_key,
        "liteos-apk-probe",
        "1.0.0-r0",
        "usr/share/liteos-apk/probe",
        "version-1\n",
        ("liteos-apk-dependency",),
    )
    probe_v2 = _build_fixture_package(
        workspace,
        private_key,
        public_key,
        "liteos-apk-probe",
        "2.0.0-r0",
        "usr/share/liteos-apk/probe",
        "version-2\n",
        ("liteos-apk-dependency",),
    )
    concurrent_a = _build_fixture_package(
        workspace,
        private_key,
        public_key,
        "liteos-apk-concurrent-a",
        "1.0.0-r0",
        "usr/share/liteos-apk/concurrent-a",
        "concurrent-a\n",
    )
    concurrent_b = _build_fixture_package(
        workspace,
        private_key,
        public_key,
        "liteos-apk-concurrent-b",
        "1.0.0-r0",
        "usr/share/liteos-apk/concurrent-b",
        "concurrent-b\n",
    )
    trusted_tamper = _build_fixture_package(
        workspace,
        private_key,
        public_key,
        "liteos-apk-tamper",
        "1.0.0-r0",
        "usr/share/liteos-apk/tamper",
        "must-not-install\n",
    )
    tampered = workspace / "liteos-apk-tamper-invalid-1.0.0-r0.apk"
    tamper_signed_apk_control(trusted_tamper, tampered)
    trusted_tamper.unlink()
    return dependency, probe_v1, probe_v2, concurrent_a, concurrent_b, tampered


def install_apk_crash_fixtures(
    image: Path,
    debugfs: Path,
    workspace: Path,
) -> tuple[str, str, str]:
    """向一次性 runtime image 注入三代 signed package，供掉电恢复竖切使用。"""
    bootstrap = cached_apk_bootstrap()
    packages = tuple(
        _build_fixture_package(
            workspace,
            bootstrap.private_key,
            bootstrap.public_key,
            "liteos-apk-crash",
            f"{version}.0.0-r0",
            "usr/share/liteos-apk/crash",
            f"crash-v{version}\n".encode() + bytes([64 + version]) * (512 * 1024),
        )
        for version in (1, 2, 3)
    )
    commands = workspace / "apk-crash.debugfs"
    commands.write_text(
        "".join(f"write {package} /run/{package.name}\n" for package in packages)
    )
    run([str(debugfs), "-w", "-f", str(commands), str(image)])
    return tuple(package.name for package in packages)


def _inject_package_bootstrap(
    image: Path,
    debugfs: Path,
    workspace: Path,
    package: Path,
    fixtures: tuple[Path, ...],
) -> None:
    """临时替换 init policy，并在同一次 guest boot 验证 package-manager contract。"""
    fixture = {path.name: path for path in fixtures}
    dependency = next(path for path in fixtures if path.name.startswith("liteos-apk-dependency-"))
    probe_v1 = next(path for path in fixtures if path.name.startswith("liteos-apk-probe-1."))
    probe_v2 = next(path for path in fixtures if path.name.startswith("liteos-apk-probe-2."))
    concurrent_a = next(path for path in fixtures if path.name.startswith("liteos-apk-concurrent-a-"))
    concurrent_b = next(path for path in fixtures if path.name.startswith("liteos-apk-concurrent-b-"))
    tampered = next(path for path in fixtures if path.name.startswith("liteos-apk-tamper-invalid-"))
    run_paths = " ".join(f"/run/{name}" for name in (package.name, *fixture))
    bootstrap_script = workspace / "apk-bootstrap.sh"
    bootstrap_script.write_text(
        "#!/bin/sh\n"
        "set -e\n"
        "APK='/sbin/apk.static --no-network'\n"
        "rm -f /etc/inittab\n"
        f"$APK --initdb add /run/{package.name}\n"
        f"if $APK add /run/{probe_v1.name}; then exit 71; fi\n"
        f"$APK add /run/{dependency.name} /run/{probe_v1.name}\n"
        "[ \"$(cat /usr/share/liteos-apk/dependency)\" = dependency ]\n"
        "[ \"$(cat /usr/share/liteos-apk/probe)\" = version-1 ]\n"
        f"$APK add --upgrade /run/{probe_v2.name}\n"
        "[ \"$(cat /usr/share/liteos-apk/probe)\" = version-2 ]\n"
        f"if $APK add /run/{tampered.name}; then exit 72; fi\n"
        "[ ! -e /usr/share/liteos-apk/tamper ]\n"
        f"$APK add /run/{concurrent_a.name} & first=$!\n"
        f"$APK add /run/{concurrent_b.name} & second=$!\n"
        "first_status=0; wait $first || first_status=$?\n"
        "second_status=0; wait $second || second_status=$?\n"
        "[ $first_status -eq 0 ] || [ $second_status -eq 0 ]\n"
        f"$APK add /run/{concurrent_a.name} /run/{concurrent_b.name}\n"
        "$APK info -e liteos-apk-concurrent-a\n"
        "$APK info -e liteos-apk-concurrent-b\n"
        "$APK del liteos-apk-probe liteos-apk-dependency "
        "liteos-apk-concurrent-a liteos-apk-concurrent-b\n"
        "[ ! -e /usr/share/liteos-apk/probe ]\n"
        "[ ! -e /usr/share/liteos-apk/dependency ]\n"
        "[ ! -e /usr/share/liteos-apk/concurrent-a ]\n"
        "[ ! -e /usr/share/liteos-apk/concurrent-b ]\n"
        f"rm -f {run_paths} /run/apk-bootstrap.sh\n"
        "/bin/sync\n"
        "echo LITEOS_APK_PACKAGE_OPERATIONS_READY\n"
        "echo LITEOS_APK_ROOTFS_READY\n"
        "while :; do /bin/sleep 1; done\n"
    )
    bootstrap_inittab = workspace / "bootstrap.inittab"
    bootstrap_inittab.write_text("::sysinit:/bin/sh /run/apk-bootstrap.sh\n")
    commands = workspace / "bootstrap.debugfs"
    commands.write_text(
        f"write {package} /run/{package.name}\n"
        + "".join(f"write {path} /run/{path.name}\n" for path in fixtures)
        + f"write {bootstrap_script} /run/apk-bootstrap.sh\n"
        "set_inode_field /run/apk-bootstrap.sh mode 0100755\n"
        "rm /etc/inittab\n"
        f"write {bootstrap_inittab} /etc/inittab\n"
    )
    run([str(debugfs), "-w", "-f", str(commands), str(image)])


def _verify_package_ownership(image: Path, debugfs: Path) -> None:
    """拒绝 package database 缺失、bootstrap 残留或 base package 未登记的镜像。"""
    installed = run([str(debugfs), "-R", "cat /lib/apk/db/installed", str(image)])
    if f"P:{BASE_PACKAGE_NAME}" not in installed or f"V:{BASE_PACKAGE_VERSION}" not in installed:
        raise RuntimeError("final rootfs is not owned by the liteos-base APK database entry")
    listing = run([str(debugfs), "-R", "ls -l /run", str(image)])
    if ".apk" in listing or "apk-bootstrap.sh" in listing:
        raise RuntimeError("final rootfs retains temporary APK bootstrap artifacts")
    for package in (
        "liteos-apk-dependency",
        "liteos-apk-probe",
        "liteos-apk-concurrent-a",
        "liteos-apk-concurrent-b",
        "liteos-apk-tamper",
    ):
        if f"P:{package}" in installed:
            raise RuntimeError(f"final rootfs retains APK gate package: {package}")


def assemble_apk_rootfs(
    image: Path,
    debugfs: Path,
    workspace: Path,
    busybox_links: tuple[str, ...],
    forbidden_markers: tuple[str, ...],
) -> Path:
    """执行 image → signed package → real apk install → final image 的唯一 assembly。"""
    private_key, public_key = _inject_bootstrap_files(image, debugfs, workspace)
    staging = _stage_package_root(image, debugfs, workspace, busybox_links)
    package = _build_base_package(staging, workspace, private_key, public_key)
    fixtures = _build_package_fixtures(workspace, private_key, public_key)
    _inject_package_bootstrap(image, debugfs, workspace, package, fixtures)
    boot(
        image,
        1,
        ("LITEOS_APK_PACKAGE_OPERATIONS_READY", "LITEOS_APK_ROOTFS_READY"),
        timeout_seconds=60,
        forbidden_markers=forbidden_markers,
        persistent_writes=True,
    )
    _verify_package_ownership(image, debugfs)
    return image
