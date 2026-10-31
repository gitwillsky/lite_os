"""用真实 ext4 验证旧系统撤销、数据保留和重试，而非只检查命令字符串。"""
from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import sync_userland
from ext4_image import find_mke2fs, run_debugfs
from userland import UserlandArtifact


class UserlandSyncTests(unittest.TestCase):
    def test_retirement_without_old_stamp_preserves_user_files_and_symlink_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            image = directory / "fs.img"
            with image.open("wb") as stream:
                stream.truncate(32 * 1024 * 1024)
            subprocess.run(
                [str(find_mke2fs()), "-q", "-t", "ext4", "-b", "4096", "-J", "size=4", str(image)],
                check=True,
            )
            source = directory / "old"
            source.write_text("old product")
            commands = [f"mkdir {path}" for path in (
                "/bin", "/etc", "/etc/init.d", "/etc/terminfo", "/etc/terminfo/l",
                "/usr", "/usr/lib", "/usr/lib/lite-runtime", "/usr/share", "/usr/share/liteos",
                "/usr/share/liteos/apps", "/usr/share/liteos/apps/terminal", "/root", "/root/project",
                "/opt", "/opt/wine", "/opt/wine/bin", "/opt/wine/lib", "/usr/lib/liteos",
                "/etc/fonts", "/etc/fonts/conf.d", "/usr/share/applications",
                "/usr/share/fontconfig", "/usr/share/fontconfig/conf.avail",
                "/usr/share/X11", "/usr/share/X11/xkb", "/usr/share/X11/xkb/rules",
            )]
            commands.extend(f'write {source} "{path}"' for path in (
                "/bin/audio-service", "/bin/desktop", "/etc/init.d/graphical-session",
                "/usr/lib/lite-runtime/runtime.js", "/usr/share/liteos/apps/terminal/main.js",
                "/usr/share/liteos/apps/terminal/old name.js", "/root/project/keep.txt",
                "/bin/wine", "/opt/wine/bin/wine", "/opt/wine/lib/libwow64fex.dll",
                "/usr/share/applications/wine-programs.desktop", "/usr/share/applications/wine-exe.desktop",
                "/usr/share/applications/wine-msi.desktop", "/etc/fonts/conf.d/99-liteos-ui.conf",
                "/usr/share/fontconfig/conf.avail/10-hinting.conf", "/usr/share/X11/xkb/rules/evdev",
            ))
            commands.append("symlink /usr/share/liteos/user-link /root/project")
            sync_userland.run_batch(image, commands, directory, "fixture", writable=True)
            config = directory / "inittab"
            config.write_text("::respawn:-/bin/sh\n")
            scene = directory / "boot-scene.lbs"
            scene.write_bytes(b"LBS1 retained fixture")
            # Windows 2000 墙纸安装名含空格；debugfs 参数必须整体传递。
            wallpaper = directory / "Blue Lace 16.bmp"
            wallpaper.write_bytes(b"BM wallpaper fixture")
            artifacts = (
                UserlandArtifact(config, "/etc/inittab"),
                UserlandArtifact(scene, "/usr/share/liteos/boot-scene.lbs"),
                UserlandArtifact(wallpaper, "/usr/share/liteos/wallpapers/Blue Lace 16.bmp"),
                # 宿主源文件没有 setuid；只能以镜像 inode 裁决，dump -p 会丢失该位。
                UserlandArtifact(source, "/bin/fusermount3", 0o4755),
                # 原生音频仍消费同名库；撤销旧运行时不能删除当前 owner 保留的文件。
                UserlandArtifact(source, "/usr/lib/liteos/libasound.so.2", 0o755),
                UserlandArtifact(config, "/etc/fonts/fonts.conf"),
            )
            # 回读失败不能发布成功 stamp；下一次必须能完成修复。
            with patch.object(sync_userland, "verify_artifacts", side_effect=RuntimeError("readback failed")):
                with self.assertRaisesRegex(RuntimeError, "readback failed"):
                    sync_userland.synchronize(image, artifacts)
            self.assertIsNone(sync_userland.read_stamp(image))
            self.assertTrue(sync_userland.synchronize(image, artifacts))
            self.assertFalse(sync_userland.synchronize(image, artifacts))
            for removed in (
                "/bin/audio-service", "/bin/desktop", "/etc/init.d/graphical-session",
                "/usr/lib/lite-runtime", "/usr/share/liteos/apps", "/usr/share/liteos/user-link",
                "/bin/wine", "/opt/wine", "/usr/share/fontconfig", "/usr/share/X11/xkb",
                "/usr/share/applications/wine-programs.desktop", "/usr/share/applications/wine-exe.desktop",
                "/usr/share/applications/wine-msi.desktop", "/etc/fonts/conf.d/99-liteos-ui.conf",
            ):
                self.assertNotIn("Inode:", run_debugfs(image, f"stat {removed}"), removed)
            self.assertIn("old product", run_debugfs(image, "cat /root/project/keep.txt"))
            self.assertIn("BM wallpaper fixture", run_debugfs(image, 'cat "/usr/share/liteos/wallpapers/Blue Lace 16.bmp"'))
            self.assertIn(config.read_text().strip(), run_debugfs(image, "cat /etc/inittab"))
            self.assertIn("root::", run_debugfs(image, "cat /etc/shadow"))
            self.assertIn("0600", run_debugfs(image, "stat /etc/shadow"))
            legacy = directory / "legacy-shadow"
            legacy.write_text("root:!:1:0:99999:7:::\nnobody:!:1:0:99999:7:::\n")
            sync_userland.run_batch(image, ["rm /etc/shadow", f"write {legacy} /etc/shadow"], directory, "legacy-default", writable=True)
            # 即使系统文件命中缓存，出厂默认迁移也必须生效并报告实际修改。
            self.assertTrue(sync_userland.synchronize(image, artifacts))
            self.assertIn("root::", run_debugfs(image, "cat /etc/shadow"))
            self.assertIn("nobody:!:", run_debugfs(image, "cat /etc/shadow"))
            self.assertFalse(sync_userland.synchronize(image, artifacts))
            password = directory / "shadow"
            password.write_text("root:$6$existing-user-hash:1:0:99999:7:::\n")
            sync_userland.run_batch(image, ["rm /etc/shadow", f"write {password} /etc/shadow", "set_inode_field /etc/shadow mode 0100600"], directory, "existing-password", writable=True)
            config.write_text(config.read_text() + "# new system generation\n")
            self.assertTrue(sync_userland.synchronize(image, artifacts))
            self.assertIn(password.read_text().strip(), run_debugfs(image, "cat /etc/shadow"))
            # 有旧 stamp 时，移除其独占库，保留当前清单共享的 ALSA 与 Fontconfig 文件。
            previous = sync_userland.read_stamp(image)
            stale_library = "/usr/lib/liteos/libxkbregistry.so.0"
            previous["identity"] = "previous-runtime"
            previous["paths"].append(stale_library)
            stamp = directory / "previous-stamp.json"
            stamp.write_text(json.dumps(previous))
            sync_userland.run_batch(image, [f"write {source} {stale_library}",
                f"rm {sync_userland.STAMP_PATH}", f"write {stamp} {sync_userland.STAMP_PATH}"],
                directory, "previous-runtime", writable=True)
            self.assertTrue(sync_userland.synchronize(image, artifacts))
            self.assertNotIn("Inode:", run_debugfs(image, f"stat {stale_library}"))
            self.assertIn("old product", run_debugfs(image, "cat /usr/lib/liteos/libasound.so.2"))
            self.assertIn(config.read_text().strip(), run_debugfs(image, "cat /etc/fonts/fonts.conf"))
            self.assertFalse(sync_userland.synchronize(image, artifacts))

    def test_managed_paths_cannot_escape_into_user_data(self):
        for path in ("/root/project", "/usr/share/liteos/../../bin/sh", "/usr/share/liteos//bad", "/etc/passwd"):
            self.assertFalse(sync_userland.managed_path(path), path)

    def test_every_product_file_is_a_managed_path(self):
        # 产品清单新增文件时，同步器的白名单必须同步更新；否则开发镜像同步会在运行时才报错。
        from userland.artifacts import base_userland

        from unittest.mock import patch, Mock
        with patch("userland.artifacts._build_rust_user_targets", side_effect=lambda _musl, targets: {
            target: Path("/nonexistent/program") for target in targets
        }):
            for artifact in base_userland(Mock()):
                self.assertTrue(sync_userland.managed_path(artifact.destination), artifact.destination)


if __name__ == "__main__":
    unittest.main()
