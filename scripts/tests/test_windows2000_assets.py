from __future__ import annotations

import importlib.util
import json
import struct
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from extract_windows2000_assets import best_icon_frame, check, digest, icon_container  # noqa: E402
from extract_windows2000_assets import publish as publish_product  # noqa: E402


class Windows2000AssetIntegrityTests(unittest.TestCase):
    def test_stale_font_extraction_cannot_remove_required_caption_symbols(self) -> None:
        stale = {"logical": {"fonts": dict.fromkeys(("simsun.ttc", "tahoma.ttf", "tahomabd.ttf"))}}
        with patch("extract_windows2000_assets.check", return_value=stale):
            with self.assertRaisesRegex(ValueError, "font extraction recipe changed"):
                publish_product(Path("unused-stale-extraction"))

    def setUp(self) -> None:
        self.work = tempfile.TemporaryDirectory()
        self.addCleanup(self.work.cleanup)
        self.directory = Path(self.work.name) / "assets"
        self.directory.mkdir()
        self.file = self.directory / "original.cur"
        self.file.write_bytes(b"original AND/XOR bytes")
        self.manifest = {
            "files": {"original.cur": {"bytes": self.file.stat().st_size,
                                       "sha256": digest(self.file)}},
            "logical": {"cursors": {"text": {"paths": ["original.cur"]}}},
        }

    def publish(self) -> None:
        (self.directory / "manifest.json").write_text(json.dumps(self.manifest))

    def test_directory_can_move_without_invalidating_identity(self) -> None:
        self.publish()
        moved = self.directory.with_name("moved")
        self.directory.rename(moved)
        self.assertEqual(check(moved), self.manifest)

    def test_same_length_corruption_is_detected(self) -> None:
        self.publish()
        self.file.write_bytes(b"X" * self.file.stat().st_size)
        with self.assertRaisesRegex(ValueError, "integrity mismatch"):
            check(self.directory)

    def test_missing_original_is_not_accepted(self) -> None:
        self.publish()
        self.file.unlink()
        with self.assertRaises(FileNotFoundError):
            check(self.directory)

    def test_manifest_cannot_reference_outside_directory(self) -> None:
        self.manifest["files"]["../outside.cur"] = self.manifest["files"].pop("original.cur")
        self.publish()
        with self.assertRaisesRegex(ValueError, "escapes asset directory"):
            check(self.directory)

    def test_logical_asset_must_reference_recorded_bytes(self) -> None:
        self.manifest["logical"]["cursors"]["text"]["paths"] = ["missing.cur"]
        self.publish()
        with self.assertRaisesRegex(ValueError, "unrecorded logical asset"):
            check(self.directory)


class IconContainerTests(unittest.TestCase):
    def test_unreferenced_exporter_memory_is_not_published(self) -> None:
        for kind in (1, 2):
            # CUR 的前八字节包含热点而不是 planes/bpp；重建必须逐字节保留。
            header = struct.pack("<HHH", 0, kind, 1)
            entry = struct.pack("<BBBBHHII", 32, 32, 0, 0, 7, 9, 3, 22)
            referenced = header + entry + b"xor"
            self.assertEqual(icon_container(referenced + b"random process memory"), referenced)
            self.assertEqual(icon_container(referenced), referenced)

    def test_directory_cannot_reference_header_or_missing_image(self) -> None:
        for offset in (0, 6, 21, 23, 0xFFFFFFFF):
            container = struct.pack("<HHHBBBBHHII", 0, 1, 1, 32, 32, 0, 0, 1, 4, 3, offset) + b"xor"
            with self.assertRaisesRegex(ValueError, "outside file"):
                icon_container(container)


if __name__ == "__main__":
    unittest.main()


def two_pixel_icon(*variants: tuple[int, bytes, bytes]) -> bytes:
    """按给定顺序拼 2×2 ICO；每项为 (bpp, 调色板, XOR 位图一行字节)。AND 掩码全 0（不透明）。"""
    bodies = []
    for bpp, palette, xor_row in variants:
        header = struct.pack("<IiiHHIIiiII", 40, 2, 4, 1, bpp, 0, 0, 0, 0, len(palette) // 4, 0)
        bodies.append((bpp, header + palette + (xor_row.ljust(4, b"\0")) * 2 + b"\0" * 8))
    offset = 6 + 16 * len(bodies)
    entries = b""
    for bpp, body in bodies:
        entries += struct.pack("<BBBBHHII", 2, 2, 0, 0, 1, bpp, len(body), offset)
        offset += len(body)
    return struct.pack("<HHH", 0, 1, len(bodies)) + entries + b"".join(body for _, body in bodies)


@unittest.skipUnless(importlib.util.find_spec("PIL"), "Pillow only exists in the local asset environment")
class IconVariantTests(unittest.TestCase):
    def test_highest_bit_depth_variant_wins_even_when_monochrome_comes_first(self) -> None:
        from PIL import Image

        mono = (1, bytes([0, 0, 0, 0, 255, 255, 255, 0]), bytes([0b11000000]))
        red = (4, bytes([0, 0, 0, 0, 0, 0, 255, 0]) + bytes(56), bytes([0x11]))
        with tempfile.TemporaryDirectory() as work:
            path = Path(work) / "icon.ico"
            path.write_bytes(two_pixel_icon(mono, red))
            with Image.open(path) as image:
                self.assertEqual([e.bpp for e in image.ico.entry], [1, 4])
                pixel = best_icon_frame(image.ico, 2).convert("RGBA").getpixel((0, 0))
        self.assertEqual(pixel, (255, 0, 0, 255))
