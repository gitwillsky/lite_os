"""产品 userland 文件安装清单；不包含门禁探针。"""
from __future__ import annotations

from build_target import target_from_environment
from classic_assets import (
    boot_scene,
    cursors,
    fonts as classic_fonts,
    shell_resources,
    sounds,
    wallpapers as classic_wallpapers,
)
import json
import shutil
from pathlib import Path

from apk_elf_closure import extract_closure
from build_cache import cache_lock, fingerprint, manifest_matches, publish_directory, sha256, temporary_directory, write_manifest
from userland import UserlandArtifact
from userland.rust import (
    SYSTEM_LIBRARIES,
    SYSTEM_LIBRARY_DIRECTORY,
    _RustUserTarget,
    _build_rust_user_targets,
)
from toolchain.musl import MuslCachePaths

ROOT = Path(__file__).resolve().parents[2]
TARGET = target_from_environment()


def base_userland(musl: MuslCachePaths) -> tuple[UserlandArtifact, ...]:
    """构建产品系统文件的唯一安装清单；构建与开发镜像同步共同消费。"""
    from ui_sdk import sdk_files
    from mesa_runtime import mesa_userland
    from mpv_runtime import mpv_userland
    from fuse_runtime import fuse_userland
    from file_type_icons import userland as file_type_icons

    program_targets = tuple(
        _RustUserTarget(crate, binary, binary, 1)
        for crate, binary in (("compositor", "compositor"), ("logon", "logon"),
                              ("desktop", "desktop"), ("file-explorer", "file-explorer"), ("smbfs", "smbfs"),
                              ("control-panel", "control-panel"), ("task-manager", "taskmgr"), ("volume-control", "sndvol"),
                              ("sound-player", "sndplay"), ("media-player", "mplay"),
                              ("notepad", "notepad"), ("device-manager", "devmgmt"), ("savedump", "savedump"),
                              ("linux-uapi", "session-launch"), ("terminal", "terminal"),
                              ("resolvd", "resolvd"), ("network-config", "ipconfig"))
    )
    library_targets = tuple(
        _RustUserTarget(package, name, f"system-library-{name}", 1, True)
        for package, name in SYSTEM_LIBRARIES
    )
    built = _build_rust_user_targets(musl, (*library_targets, *program_targets))
    programs = tuple(UserlandArtifact(built[target], f"/bin/{target.name}", 0o755) for target in program_targets)
    libraries = [(built[target], target.artifact) for target in library_targets]
    # SDK 链接名和 SONAME 指向同一发布文件，旧进程仍持有原 inode。
    ui = next(path for path, name in libraries if name == "libliteos_ui.so")
    libraries.append((ui, "libliteos_ui.so.1"))
    fonts = tuple(UserlandArtifact(path, f"/usr/share/liteos/fonts/{path.name}")
                  for path in classic_fonts())
    ui = tuple(UserlandArtifact(path, f"/usr/share/liteos/ui/{path.name}")
               for path in shell_resources())
    # 系统墙纸目录由 Git 持有的原创壁纸组成，显示属性按文件名列出。
    wallpapers = tuple(UserlandArtifact(path, f"/usr/share/liteos/wallpapers/{path.name}")
                       for path in classic_wallpapers())
    return (
        UserlandArtifact(ROOT / "user/base/shutdown", "/bin/shutdown", 0o755),
        UserlandArtifact(ROOT / "user/base/rcS", "/etc/init.d/rcS", 0o755),
        UserlandArtifact(ROOT / "user/base/inittab", "/etc/inittab"),
        UserlandArtifact(ROOT / "user/base/profile", "/etc/profile"),
        UserlandArtifact(boot_scene(), "/usr/share/liteos/boot-scene.lbs"),
        UserlandArtifact(cursors(), "/usr/share/liteos/cursors.lcs"),
        UserlandArtifact(ROOT / "assets/fonts/liteos-terminal.a8", "/usr/share/liteos/liteos-terminal.a8"),
        UserlandArtifact(ROOT / "user/base/display-service", "/etc/init.d/display-service", 0o755),
        # 桌面条目：user/base 下的每个 .desktop 都安装；窗口图标、文件关联与控制面板入口都由它们声明。
        *(UserlandArtifact(path, f"/usr/share/applications/{path.name}")
          for path in sorted((ROOT / "user/base").glob("*.desktop"))),
        *(UserlandArtifact(path, f"{SYSTEM_LIBRARY_DIRECTORY}/{name}", 0o755)
          for path, name in libraries),
        UserlandArtifact(ROOT / "user/base/fonts.conf", "/etc/fonts/fonts.conf"),
        UserlandArtifact(ROOT / "user/base/mime/packages/liteos.xml", "/usr/share/mime/packages/liteos.xml"),
        UserlandArtifact(ROOT / "user/base/ld-musl.path", f"/etc/{Path(TARGET.musl_loader).name.removesuffix('.so.1')}.path"),
        *programs, *fonts, *ui, *wallpapers, *mesa_userland(), *mpv_userland(), *fuse_userland(), *file_type_icons(),
        *_openssh_userland(),
        *(UserlandArtifact(path, destination, mode) for path, destination, mode in sdk_files()),
        *(UserlandArtifact(path, f"/usr/share/liteos/sounds/{name}") for name, path in sounds()),
    )


OPENSSH_PACKAGES = {
    "openssh-server-10.0_p1-r11.apk": (
        "bf9fd3fb8840c679699ca57b3953e853db28613422f2229e9c1b0e382ee50f0d",
        "a1d8d29c4dc64e61b1ecf2ef8e10a23ad8852e0f81b59916dfa73cfc1cf5eeb1",
    ),
    "openssh-keygen-10.0_p1-r11.apk": (
        "876102399f823493106775d262ae9a0a02cd27ceffeedc227d4f9b1ed8ecebf5",
        "0f785298eb0652b580bea1c9d7c96e2d5ad528f40f9de6367ecd7f432c0239cd",
    ),
    "libcrypto3-3.5.9-r0.apk": (
        "ede2ef7eca3b65a42e902aaf7890f9c5647dca0c55123a817179ad9f3bc6ef24",
        "2bb9af0e10adf4620074c9315c0db57dcd8b1c0492172d6782f59bb20e122cd0",
    ),
    "zlib-1.3.2-r1.apk": (
        "2d274cb1b2a36d0e8347d802e6dea8648cab952ea5fae1989ab72e725842a2c3",
        "9dfa50a52957f9d04b90b70c9fdfb08e1adca326fc57a3552d6e41047434e02c",
    ),
}
# OpenSSH 以编译时绝对路径启动两个 privilege-separation 子程序。
OPENSSH_SEEDS = {"/" + path: path for path in (
    "usr/sbin/sshd", "usr/lib/ssh/sshd-auth", "usr/lib/ssh/sshd-session", "usr/bin/ssh-keygen",
)}


def _openssh_userland() -> tuple[UserlandArtifact, ...]:
    """发布经过 SHA-256、架构和依赖闭包校验的诊断服务文件。"""
    target = target_from_environment()
    index = {"aarch64": 0, "riscv64": 1}[target.arch]
    packages = [{"name": name, "sha256": hashes[index]} for name, hashes in OPENSSH_PACKAGES.items()]
    payload = {"arch": target.arch, "packages": packages, "seeds": OPENSSH_SEEDS}
    cache = ROOT / "target" / "openssh-runtime" / target.arch
    final = cache / fingerprint(payload)
    with cache_lock(cache / ".lock"):
        if not manifest_matches(final, payload, ("files.json",)):
            temporary = temporary_directory(cache, "runtime")
            try:
                extract_closure(packages, OPENSSH_SEEDS, temporary, label="OpenSSH",
                                machine=("AArch64", "RISC-V")[index])
                write_manifest(temporary, payload)
                publish_directory(temporary, final)
            finally:
                shutil.rmtree(temporary, ignore_errors=True)
    entries = json.loads((final / "files.json").read_text())
    files = []
    for entry in entries:
        path = final / entry["path"]
        if sha256(path) != entry["sha256"]:
            raise RuntimeError(f"corrupt OpenSSH runtime cache: {path}")
        files.append(UserlandArtifact(path, "/" + entry["path"], entry["mode"]))
    return (*files,
            UserlandArtifact(ROOT / "user/base/sshd", "/etc/init.d/sshd", 0o755),
            UserlandArtifact(ROOT / "user/base/sshd_config", "/etc/ssh/sshd_config"))
