"""用真实 ext4 验证旧系统撤销、数据保留和重试，而非只检查命令字符串。"""
from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import sync_userland
from ext4_image import find_mke2fs, run_debugfs
from verify_busybox import UserlandArtifact


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
            )]
            commands.extend(f"write {source} {path}" for path in (
                "/bin/audio-service", "/bin/desktop", "/etc/init.d/graphical-session",
                "/usr/lib/lite-runtime/runtime.js", "/usr/share/liteos/apps/terminal/main.js",
                "/root/project/keep.txt",
            ))
            commands.append("symlink /usr/share/liteos/user-link /root/project")
            sync_userland.run_batch(image, commands, directory, "fixture", writable=True)
            config = directory / "inittab"
            config.write_text("::respawn:-/bin/sh\n")
            scene = directory / "boot-scene.lbs"
            scene.write_bytes(b"LBS1 retained fixture")
            artifacts = (
                UserlandArtifact(config, "/etc/inittab"),
                UserlandArtifact(scene, "/usr/share/liteos/boot-scene.lbs"),
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
            ):
                self.assertNotIn("Inode:", run_debugfs(image, f"stat {removed}"), removed)
            self.assertIn("old product", run_debugfs(image, "cat /root/project/keep.txt"))
            self.assertIn(config.read_text().strip(), run_debugfs(image, "cat /etc/inittab"))

    def test_managed_paths_cannot_escape_into_user_data(self):
        for path in ("/root/project", "/usr/share/liteos/../../bin/sh", "/usr/share/liteos//bad", "/etc/passwd"):
            self.assertFalse(sync_userland.managed_path(path), path)

    def test_every_product_file_is_a_managed_path(self):
        # 产品清单新增文件时，同步器的白名单必须同步更新；否则开发镜像同步会在运行时才报错。
        from verify_busybox import base_userland

        for artifact in base_userland(Path("/nonexistent/compositor")):
            self.assertTrue(sync_userland.managed_path(artifact.destination), artifact.destination)


if __name__ == "__main__":
    unittest.main()
