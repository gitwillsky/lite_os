"""产品 userland 文件安装清单；不包含门禁探针。"""
from __future__ import annotations

from build_target import target_from_environment
from classic_assets import (
    boot_scene,
    cursors,
    fonts as classic_fonts,
    shell_bitmaps,
    sounds,
    wallpapers as classic_wallpapers,
)
from pathlib import Path
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
                              ("notepad", "notepad"), ("device-manager", "devmgmt"),
                              ("linux-uapi", "session-launch"), ("terminal", "terminal"))
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
               for path in shell_bitmaps())
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
        UserlandArtifact(ROOT / "user/base/network-service", "/etc/init.d/network-service", 0o755),
        UserlandArtifact(ROOT / "user/base/udhcpc.script", "/usr/share/udhcpc/default.script", 0o755),
        # 桌面条目：user/base 下的每个 .desktop 都安装；窗口图标、文件关联与控制面板入口都由它们声明。
        *(UserlandArtifact(path, f"/usr/share/applications/{path.name}")
          for path in sorted((ROOT / "user/base").glob("*.desktop"))),
        *(UserlandArtifact(path, f"{SYSTEM_LIBRARY_DIRECTORY}/{name}", 0o755)
          for path, name in libraries),
        UserlandArtifact(ROOT / "user/base/fonts.conf", "/etc/fonts/fonts.conf"),
        UserlandArtifact(ROOT / "user/base/mime/packages/liteos.xml", "/usr/share/mime/packages/liteos.xml"),
        UserlandArtifact(ROOT / "user/base/ld-musl.path", f"/etc/{Path(TARGET.musl_loader).name.removesuffix('.so.1')}.path"),
        *programs, *fonts, *ui, *wallpapers, *mesa_userland(), *mpv_userland(), *fuse_userland(), *file_type_icons(),
        *(UserlandArtifact(path, destination, mode) for path, destination, mode in sdk_files()),
        *(UserlandArtifact(path, f"/usr/share/liteos/sounds/{name}") for name, path in sounds()),
    )
