from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import classic_assets  # noqa: E402


class BootSceneSelectionTests(unittest.TestCase):
    def test_local_original_wins_when_it_has_been_packed(self) -> None:
        with tempfile.TemporaryDirectory() as work:
            original = Path(work) / "boot-scene.lbs"
            original.write_bytes(b"LBS1")
            with mock.patch.object(classic_assets, "ORIGINAL_BOOT_SCENE", original):
                self.assertEqual(classic_assets.boot_scene(), original)

    def test_committed_default_is_used_when_no_original_exists(self) -> None:
        with tempfile.TemporaryDirectory() as work:
            with mock.patch.object(classic_assets, "ORIGINAL_BOOT_SCENE", Path(work) / "missing.lbs"):
                self.assertEqual(classic_assets.boot_scene(), classic_assets.DEFAULT_BOOT_SCENE)

    def test_a_directory_at_the_original_path_is_not_mistaken_for_the_asset(self) -> None:
        with tempfile.TemporaryDirectory() as work:
            with mock.patch.object(classic_assets, "ORIGINAL_BOOT_SCENE", Path(work)):
                self.assertEqual(classic_assets.boot_scene(), classic_assets.DEFAULT_BOOT_SCENE)

    def test_the_committed_default_exists_so_plain_builds_need_no_local_assets(self) -> None:
        self.assertTrue(classic_assets.DEFAULT_BOOT_SCENE.is_file())


if __name__ == "__main__":
    unittest.main()
