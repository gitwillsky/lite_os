from __future__ import annotations

import sys
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import pack_bootfs  # noqa: E402


def _members(archive: bytes) -> dict[bytes, tuple[int, int]]:
    """按标准 cpio newc 规则解析：返回 名称 → (内容偏移, 长度)；名称在第一个 NUL 处结束。"""
    result: dict[bytes, tuple[int, int]] = {}
    offset = 0
    while True:
        header = archive[offset : offset + 110]
        assert header[:6] == b"070701"
        size = int(header[54:62], 16)
        name_size = int(header[94:102], 16)
        name = archive[offset + 110 : offset + 110 + name_size].split(b"\0")[0]
        data = (offset + 110 + name_size + 3) & ~3
        if name == b"TRAILER!!!":
            return result
        result[name] = (data, size)
        offset = (data + size + 3) & ~3


class BootfsPackingTests(unittest.TestCase):
    def test_file_contents_are_page_aligned_so_they_can_be_shared_copy_on_write(self) -> None:
        archive = bytearray()
        files = {"bin/a": b"a" * 5000, "data/b": b"hello", "bin/empty": b""}
        for inode, (name, data) in enumerate(files.items(), start=1):
            archive += pack_bootfs._member(name, 0o100644, data, inode, len(archive))
        archive += pack_bootfs._member("TRAILER!!!", 0, b"", 0, len(archive))
        parsed = _members(bytes(archive))
        for name, data in files.items():
            offset, size = parsed[name.encode()]
            self.assertEqual(size, len(data))
            self.assertEqual(bytes(archive[offset : offset + size]), data)
            if data:
                self.assertEqual(offset % pack_bootfs.PAGE, 0, name)

    def test_packing_is_deterministic(self) -> None:
        first = pack_bootfs._member("bin/x", 0o100755, b"payload", 7, 4096)
        self.assertEqual(first, pack_bootfs._member("bin/x", 0o100755, b"payload", 7, 4096))


if __name__ == "__main__":
    unittest.main()
