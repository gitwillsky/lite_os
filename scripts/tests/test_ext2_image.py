from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from ext2_image import (  # noqa: E402
    ensure_ext2_capacity,
    ext2_capacity_bytes,
    find_mke2fs,
)


class Ext2ImageTests(unittest.TestCase):
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
                    "ext2",
                    "-b",
                    "4096",
                    "-I",
                    "256",
                    "-O",
                    "^ext_attr,^resize_inode,^dir_index,filetype,sparse_super,large_file,has_journal",
                    "-J",
                    "size=4",
                    str(image),
                ],
                check=True,
            )

            ensure_ext2_capacity(image, 32)

            expected = 32 * 1024 * 1024
            self.assertEqual(image.stat().st_size, expected)
            self.assertEqual(ext2_capacity_bytes(image), expected)


if __name__ == "__main__":
    unittest.main()
