from __future__ import annotations

import struct
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from pack_boot_scene import HEIGHT, WIDTH, pack, unpack  # noqa: E402


class FakeImage:
    """只提供 pack 用到的 size/convert/tobytes，使测试不依赖 Pillow。"""

    def __init__(self, pixels: list[tuple[int, int, int]], size: tuple[int, int] = (WIDTH, HEIGHT)) -> None:
        self.size, self.pixels = size, pixels

    def convert(self, mode: str) -> "FakeImage":
        assert mode == "RGB"
        return self

    def tobytes(self) -> bytes:
        return bytes(channel for pixel in self.pixels for channel in pixel)


def scene(fill=lambda x, y: (255, 255, 255)) -> FakeImage:
    return FakeImage([fill(x, y) for y in range(HEIGHT) for x in range(WIDTH)])


class BootSceneFormatTests(unittest.TestCase):
    def test_round_trip_preserves_every_pixel_and_splits_long_runs(self) -> None:
        # 整幅同色会产生远超 255 的游程，必须按 255 切开且解码后逐像素一致。
        image = scene(lambda x, y: (0, 41, 156) if y >= 426 else (255, 255, 255))
        width, height, pixels = unpack(pack(image))
        self.assertEqual((width, height), (WIDTH, HEIGHT))
        self.assertEqual(pixels, image.pixels)

    def test_header_carries_the_progress_slot_so_consumers_do_not_hard_code_it(self) -> None:
        data = pack(scene())
        self.assertEqual(struct.unpack_from("<HHHH", data, 12), (274, 437, 163, 8))
        self.assertEqual(struct.unpack_from("<I", data, 20)[0], 0x00299C)

    def test_more_than_256_colors_is_rejected_instead_of_silently_reduced(self) -> None:
        image = scene(lambda x, y: (x % 256, (x // 256) % 256, y % 2) if y == 0 else (1, 1, 1))
        with self.assertRaisesRegex(ValueError, "limit is 256"):
            pack(image)

    def test_wrong_size_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "must be 640x480"):
            pack(FakeImage([(0, 0, 0)] * 4, (2, 2)))

    def test_decoder_rejects_truncated_trailing_and_out_of_palette_data(self) -> None:
        data = pack(scene())
        with self.assertRaises(ValueError):
            unpack(data[:-2])  # 少一对 RLE，像素总数不足
        with self.assertRaises(ValueError):
            unpack(data + b"\x01\x00")  # 多一对，像素总数超出
        with self.assertRaises(ValueError):
            unpack(data[:-1] + b"\x07")  # 调色板只有 1 项，索引 7 越界
        with self.assertRaises(ValueError):
            unpack(b"XXXX" + data[4:])


if __name__ == "__main__":
    unittest.main()
