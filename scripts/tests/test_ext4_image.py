from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from ext4_image import (  # noqa: E402
    ensure_ext4_capacity,
    ext4_capacity_bytes,
    find_mke2fs,
)


class Ext4ImageTests(unittest.TestCase):
    def test_ensure_capacity_expands_backing_file_and_filesystem(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            image = Path(directory) / "rootfs.img"
            with image.open("wb") as stream:
                stream.truncate(16 * 1024 * 1024)
            subprocess.run(
                [
                    str(find_mke2fs()),
                    "-q",
                    "-t",
                    "ext4",
                    "-b",
                    "4096",
                    "-J",
                    "size=4",
                    str(image),
                ],
                check=True,
            )

            ensure_ext4_capacity(image, 32)

            expected = 32 * 1024 * 1024
            self.assertEqual(image.stat().st_size, expected)
            self.assertEqual(ext4_capacity_bytes(image), expected)


if __name__ == "__main__":
    unittest.main()
