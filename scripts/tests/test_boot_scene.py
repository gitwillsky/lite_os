from __future__ import annotations

import struct
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from pack_boot_scene import EXACT_COLORS, HEIGHT, WIDTH, pack, reduce_colors, unpack  # noqa: E402


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

    def test_reduce_is_explicit_keeps_exact_colors_and_merges_the_rest_to_nearest(self) -> None:
        # 抗锯齿渐变带 300 个相近色 + 必须保留的槽边框色：reduce 后总数不超限，且槽边框色逐值不变。
        def pixel(x: int, y: int) -> tuple[int, int, int]:
            if y == 0:
                return (x % 300 // 2 + 1, 0, x % 2)
            return {1: (128, 128, 128), 2: (255, 255, 255), 3: (192, 192, 192)}.get(y % 4, (0, 41, 156))

        image = scene(pixel)
        with self.assertRaisesRegex(ValueError, "--reduce"):
            pack(image)
        _, _, decoded = unpack(pack(image, reduce=True))
        self.assertLessEqual(len(set(decoded)), 256)
        for y in (1, 2, 3, 4):
            self.assertEqual(decoded[y * WIDTH], pixel(0, y))
        self.assertTrue(all(c in set(decoded) for c in EXACT_COLORS if c in set(image.pixels)))

    def test_reduce_is_a_no_op_when_the_image_already_fits(self) -> None:
        pixels = [(10, 20, 30)] * 8
        self.assertIs(reduce_colors(pixels), pixels)

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
