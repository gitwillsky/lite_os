from __future__ import annotations

import struct
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from pack_cursors import FRAME_MS, SIZES, pack as encode, unpack  # noqa: E402


class FakeImage:
    """只提供 pack 用到的 size/convert/tobytes，使测试不依赖 Pillow。"""

    def __init__(self, size: int, rgba: tuple[int, int, int, int] = (0, 0, 0, 0), pixels=None) -> None:
        self.size = (size, size)
        self.pixels = pixels or [rgba] * (size * size)

    def convert(self, mode: str) -> "FakeImage":
        assert mode == "RGBA"
        return self

    def tobytes(self) -> bytes:
        return bytes(channel for pixel in self.pixels for channel in pixel)


def cursor(frames: int = 1, rgba=(255, 0, 0, 255)) -> dict[int, list]:
    return {size: [FakeImage(size, rgba) for _ in range(frames)] for size in SIZES}


def pack(cursors):
    return encode(cursors, {"arrow": (0, 0), "busy": (16, 16), "link": (10, 10)})


class CursorSetTests(unittest.TestCase):
    def test_round_trip_keeps_frames_hotspots_and_sizes(self) -> None:
        decoded = unpack(pack({"arrow": cursor(), "busy": cursor(frames=3), "link": cursor()}))
        self.assertEqual(sorted(decoded), ["arrow", "busy", "link"])
        self.assertEqual(decoded["arrow"]["hotspot"], {32: (0, 0), 64: (0, 0)})
        self.assertEqual(decoded["link"]["hotspot"], {32: (10, 10), 64: (20, 20)})
        self.assertEqual(decoded["busy"]["hotspot"][64], (32, 32))
        self.assertEqual((decoded["busy"]["frames"], decoded["busy"]["frame_ms"]), (3, FRAME_MS))
        self.assertEqual((decoded["arrow"]["frames"], decoded["arrow"]["frame_ms"]), (1, 0))
        self.assertEqual(len(decoded["busy"]["pixels"][32]), 3 * 32 * 32)
        self.assertEqual(len(decoded["busy"]["pixels"][64]), 3 * 64 * 64)

    def test_pixels_are_premultiplied_and_rounded(self) -> None:
        pixels = [(255, 128, 0, 128)] + [(10, 20, 30, 0)] * (32 * 32 - 1)
        image = FakeImage(32, pixels=pixels)
        decoded = unpack(pack({"arrow": {32: [image], 64: [FakeImage(64, (0, 0, 0, 255))]}}))
        first = decoded["arrow"]["pixels"][32][0]
        self.assertEqual(first, 0x80 << 24 | 128 << 16 | 64 << 8 | 0)
        # 全透明像素的颜色分量必须清零，否则预乘约定被破坏。
        self.assertEqual(decoded["arrow"]["pixels"][32][1], 0)
        self.assertEqual(decoded["arrow"]["pixels"][64][0], 0xFF000000)

    def test_inconsistent_sets_are_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "no hotspot"):
            pack({"mystery": cursor()})
        with self.assertRaisesRegex(ValueError, "exactly the sizes"):
            pack({"arrow": {32: [FakeImage(32)]}})
        with self.assertRaisesRegex(ValueError, "frame counts differ"):
            pack({"busy": {32: [FakeImage(32)] * 2, 64: [FakeImage(64)]}})
        with self.assertRaisesRegex(ValueError, "a 32px frame is 30x30"):
            pack({"arrow": {32: [FakeImage(30)], 64: [FakeImage(64)]}})

    def test_decoder_rejects_corruption(self) -> None:
        data = pack({"arrow": cursor()})
        with self.assertRaises(ValueError):
            unpack(b"XXXX" + data[4:])
        with self.assertRaises(ValueError):
            unpack(data + b"\0")
        with self.assertRaises(struct.error):
            unpack(data[:-4])

    def test_repository_set_has_original_hotspots_and_integer_scaling(self) -> None:
        from classic_assets import cursors
        decoded = unpack(cursors().read_bytes())
        self.assertEqual(decoded["text"]["hotspot"], {32: (15, 16), 64: (30, 32)})
        self.assertEqual(decoded["link"]["hotspot"], {32: (10, 10), 64: (20, 20)})
        for name, cursor in decoded.items():
            self.assertEqual(cursor["frames"], 1, name)
            original = cursor["pixels"][32]
            scaled = cursor["pixels"][64]
            for y in range(64):
                for x in range(64):
                    self.assertEqual(scaled[y * 64 + x], original[y // 2 * 32 + x // 2], name)


if __name__ == "__main__":
    unittest.main()
