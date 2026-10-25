from __future__ import annotations

import fcntl
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import build_cache
import prune_build_cache
import verify_apk_apps
import verify_busybox


class CacheRetentionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def rootfs(self, work: Path, number: int) -> Path:
        entry = work / "rootfs-images" / f"{number:064x}"
        entry.mkdir(parents=True)
        (entry / "fs.img").write_bytes(b"rootfs")
        (entry / build_cache.CACHE_MANIFEST).write_text("{}")
        os.utime(entry, ns=(number, number))
        return entry

    def apk(self, work: Path, number: int) -> Path:
        work.mkdir(parents=True, exist_ok=True)
        image = work / f"installed-{number:064x}.img"
        image.write_bytes(b"installed")
        image.with_suffix(".json").write_text("{}")
        os.utime(image, ns=(number, number))
        return image

    def test_old_current_rootfs_is_retained_with_newest_other(self) -> None:
        entries = [self.rootfs(self.root, number) for number in range(1, 5)]
        expired = build_cache.prune_rootfs_images(self.root, current=entries[0])
        self.assertEqual(set(expired), set(entries[1:3]))
        self.assertTrue(entries[0].exists())
        self.assertTrue(entries[3].exists())

    def test_rootfs_dry_run_is_identical_to_apply_and_preserves_other_files(self) -> None:
        entries = [self.rootfs(self.root, number) for number in range(1, 5)]
        partial = entries[0].parent / f"{10:064x}"
        partial.mkdir()
        (partial / "fs.img").write_bytes(b"unfinished")
        staging = entries[0].parent / ".rootfs.tmp"
        staging.mkdir()
        outside = self.root / "fs-aarch64.img"
        outside.write_bytes(b"development")
        link = entries[0].parent / f"{11:064x}"
        link.symlink_to(self.root, target_is_directory=True)
        planned = build_cache.prune_rootfs_images(self.root, dry_run=True)
        self.assertEqual(set(planned), set(entries[:2]))
        self.assertTrue(all(path.exists() for path in entries))
        self.assertEqual(build_cache.prune_rootfs_images(self.root), planned)
        self.assertTrue(partial.is_dir())
        self.assertTrue(staging.is_dir())
        self.assertTrue(link.is_symlink())
        self.assertEqual(outside.read_bytes(), b"development")

    def test_apk_removes_image_and_stamp_but_retains_old_current(self) -> None:
        images = [self.apk(self.root, number) for number in range(1, 5)]
        planned = build_cache.prune_apk_images(self.root, current=images[0], dry_run=True)
        self.assertEqual(set(planned), {path for image in images[1:3]
                                      for path in (image, image.with_suffix(".json"))})
        self.assertTrue(all(path.exists() for path in planned))
        self.assertEqual(build_cache.prune_apk_images(self.root, current=images[0]), planned)
        self.assertTrue(all(not path.exists() for path in planned))
        self.assertTrue(images[0].exists())
        self.assertTrue(images[3].exists())

    def test_apk_does_not_follow_links_or_delete_partial_publication(self) -> None:
        images = [self.apk(self.root, number) for number in range(1, 5)]
        outside = self.root / "development.img"
        outside.write_bytes(b"keep")
        images[0].unlink()
        images[0].symlink_to(outside)
        images[1].with_suffix(".json").unlink()
        images[1].with_suffix(".json").symlink_to(outside)
        partial = self.root / f"installed-{10:064x}.img"
        partial.write_bytes(b"unpublished")
        self.assertEqual(build_cache.prune_apk_images(self.root), ())
        self.assertEqual(outside.read_bytes(), b"keep")
        self.assertTrue(partial.exists())

    def test_apk_cache_hit_runs_retention_with_the_hit_as_current(self) -> None:
        images = [self.apk(self.root, number) for number in range(1, 5)]
        with (
            patch.object(verify_apk_apps, "WORK", self.root),
            patch.object(verify_apk_apps, "cached_application_apks", return_value=Mock(archives=())),
            patch.object(verify_apk_apps, "target_runtime_artifacts", return_value=()),
            patch.object(verify_apk_apps, "runtime_gate_payload", return_value={}),
            patch.object(verify_apk_apps, "fingerprint", return_value=f"{1:064x}"),
            patch.object(verify_apk_apps, "runtime_gate_hit", return_value=True),
            build_cache.cache_lock(self.root / ".installed.lock"),
        ):
            result = verify_apk_apps.install_applications(self.root / "base.img", self.root)
        self.assertEqual(result, images[0])
        self.assertTrue(images[0].exists())
        self.assertTrue(images[3].exists())
        self.assertFalse(images[1].exists())
        self.assertFalse(images[2].exists())

    def test_manual_prune_is_arch_scoped_and_keeps_published_outputs(self) -> None:
        for arch in ("aarch64", "riscv64"):
            for number in range(1, 4):
                self.rootfs(self.root / "target/busybox-runtime" / arch, number)
                self.apk(self.root / "target/apk-apps-runtime" / arch, number)
        output = self.root / "target/rootfs/aarch64.img"
        output.parent.mkdir()
        output.write_bytes(b"published")
        dev = self.root / "fs-aarch64.img"
        dev.write_bytes(b"user data")
        with patch.object(prune_build_cache, "ROOT", self.root), redirect_stdout(StringIO()):
            planned_bytes = prune_build_cache.prune()
            self.assertGreater(planned_bytes, 0)
            self.assertEqual(prune_build_cache.prune(apply=True), planned_bytes)
            self.assertEqual(prune_build_cache.prune(), 0)
        self.assertEqual(output.read_bytes(), b"published")
        self.assertEqual(dev.read_bytes(), b"user data")
        for arch in ("aarch64", "riscv64"):
            self.assertEqual(len(list((self.root / "target/busybox-runtime" / arch / "rootfs-images").iterdir())), 2)
            self.assertEqual(len(list((self.root / "target/apk-apps-runtime" / arch).glob("*.img"))), 2)

    def test_manual_cleanup_holds_owner_lock_and_releases_it_on_failure(self) -> None:
        work = self.root / "target/busybox-runtime/aarch64"
        work.mkdir(parents=True)
        def blocked(*_args, **_kwargs):
            with (work / ".rootfs.lock").open("a+") as other:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
            raise OSError("fixture deletion failure")
        with patch.object(prune_build_cache, "ROOT", self.root), patch.object(
            prune_build_cache, "prune_rootfs_images", side_effect=blocked,
        ):
            with self.assertRaisesRegex(OSError, "fixture deletion failure"):
                prune_build_cache.prune(apply=True)
        with (work / ".rootfs.lock").open("a+") as other:
            fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_apk_gate_holds_cache_lock_through_consumer_copy(self) -> None:
        work = self.root / "apk-cache"
        base = self.root / "base.img"
        base.write_bytes(b"base")
        installed = self.apk(work, 1)
        output = self.root / "output.img"
        copy = verify_apk_apps.shutil.copyfile
        def guarded_copy(source, destination):
            with (work / ".installed.lock").open("a+") as other:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return copy(source, destination)
        with (
            patch.object(verify_apk_apps, "WORK", work),
            patch.object(sys, "argv", ["verify_apk_apps.py", "--build-only", "--image", str(base), "--output", str(output)]),
            patch.object(verify_apk_apps, "install_applications", return_value=installed),
            patch.object(verify_apk_apps.shutil, "copyfile", side_effect=guarded_copy),
            redirect_stdout(StringIO()),
        ):
            self.assertEqual(verify_apk_apps.main(), 0)
        self.assertEqual(output.read_bytes(), b"installed")

    def test_rootfs_is_copied_under_lock_before_old_images_are_pruned(self) -> None:
        work = self.root / "busybox"
        old = [self.rootfs(work, number) for number in range(1, 4)]
        output = self.root / "published.img"
        artifact = self.root / "artifact"
        keys = self.root / "keys"
        keys.mkdir()
        bootstrap = Mock(alpine_keys=keys, apk_static=artifact,
                         ca_certificates_bundle=artifact, private_key=artifact, public_key=artifact)
        def guarded_prune(directory, **kwargs):
            self.assertEqual(output.read_bytes(), b"new rootfs")
            with (work / ".rootfs.lock").open("a+") as other:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return build_cache.prune_rootfs_images(directory, **kwargs)
        def create_image(_binary, _musl, _openssl, destination):
            destination.write_bytes(b"new rootfs")
        with (
            patch.object(verify_busybox, "WORK", work),
            patch.object(verify_busybox, "base_userland", return_value=()),
            patch.object(verify_busybox, "build_stress_tools", return_value=artifact),
            patch.object(verify_busybox, "cached_apk_bootstrap", return_value=bootstrap),
            patch.object(verify_busybox, "target_runtime_artifacts", return_value=()),
            patch.object(verify_busybox, "boot_scene", return_value=artifact),
            patch.object(verify_busybox, "sha256", return_value="digest"),
            patch.object(verify_busybox, "find_mke2fs", return_value=artifact),
            patch.object(verify_busybox, "find_debugfs", return_value=artifact),
            patch.object(verify_busybox.shutil, "which", return_value="/bin/openssl"),
            patch.object(verify_busybox, "create_image", side_effect=create_image) as create,
            patch.object(verify_busybox, "prune_rootfs_images", side_effect=guarded_prune),
            redirect_stdout(StringIO()),
        ):
            musl = Mock(install=self.root)
            openssl = Mock(binary=artifact)
            self.assertEqual(verify_busybox.create_published_image(artifact, musl, openssl, output), output)
            # 命中时同样先完成 consumer 的独立复制，再应用保留策略。
            output.unlink()
            self.assertEqual(verify_busybox.create_published_image(artifact, musl, openssl, output), output)
            create.assert_called_once()
            survivors = set((work / "rootfs-images").iterdir())
            with patch.object(verify_busybox, "fingerprint", return_value="f" * 64), patch.object(
                verify_busybox, "create_image", side_effect=RuntimeError("assembly failed"),
            ):
                with self.assertRaisesRegex(RuntimeError, "assembly failed"):
                    verify_busybox.create_published_image(artifact, musl, openssl, output)
            self.assertEqual(set((work / "rootfs-images").iterdir()), survivors)
            self.assertEqual(output.read_bytes(), b"new rootfs")
        self.assertFalse(old[0].exists())
        self.assertFalse(old[1].exists())
        self.assertTrue(old[2].exists())


if __name__ == "__main__":
    unittest.main()
