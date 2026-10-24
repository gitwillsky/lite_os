from __future__ import annotations

import struct
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from pack_boot_scene import HEIGHT, WIDTH, pack  # noqa: E402
from test_boot_scene import FakeImage  # noqa: E402
from display_gate import read_ppm  # noqa: E402
from verify_display import HANDOFF_PERMILLE, expected_frame, first_difference  # noqa: E402

WHITE, BLUE = (255, 255, 255), (0, 41, 156)


def scene() -> bytes:
    pixels = [BLUE if y >= 426 else WHITE for y in range(HEIGHT) for _ in range(WIDTH)]
    return pack(FakeImage(pixels))


class ExpectedFrameTests(unittest.TestCase):
    def test_scene_is_centered_at_integer_scale_on_a_black_screen(self) -> None:
        frame = expected_frame(scene(), 1280, 800)
        at = lambda x, y: frame[y * 1280 + x]
        self.assertEqual(at(0, 0), (0, 0, 0))
        self.assertEqual(at(319, 400), (0, 0, 0))
        self.assertEqual(at(320, 160), WHITE)
        self.assertEqual(at(959, 639), BLUE)
        self.assertEqual(at(960, 400), (0, 0, 0))
        self.assertEqual(at(640, 799), (0, 0, 0))

    def test_scale_two_repeats_every_source_pixel_in_a_square(self) -> None:
        frame = expected_frame(scene(), 1280 + 40, 960 + 20)
        width = 1280 + 40
        left, top = 20, 10
        self.assertEqual(frame[top * width + left], WHITE)
        self.assertEqual(frame[(top + 2 * 426) * width + left + 5], BLUE)
        self.assertEqual(frame[(top + 2 * 426 - 1) * width + left + 5], WHITE)

    def test_progress_fill_covers_the_documented_share_of_the_slot(self) -> None:
        fill = (0, 41, 156)
        # 槽内部 (274,437) 起、宽 163；400‰ → 65 像素。底栏本身也是蓝色，所以用整幅白色场景才能分辨填充。
        white = pack(FakeImage([WHITE] * (WIDTH * HEIGHT)))
        frame = expected_frame(white, 640, 480)
        row = frame[437 * 640:438 * 640]
        filled = [x for x, pixel in enumerate(row) if pixel == fill]
        self.assertEqual((filled[0], len(filled)), (274, 163 * HANDOFF_PERMILLE // 1000))
        self.assertEqual(frame[436 * 640 + 274], WHITE)
        self.assertEqual(frame[445 * 640 + 274], WHITE)

    def test_screen_smaller_than_the_scene_is_an_error(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "smaller"):
            expected_frame(scene(), 639, 480)


class PpmTests(unittest.TestCase):
    def write(self, header: bytes, body: bytes) -> Path:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / "screen.ppm"
        path.write_bytes(header + body)
        return path

    def test_reads_a_p6_dump(self) -> None:
        path = self.write(b"P6\n2 1\n255\n", bytes([1, 2, 3, 4, 5, 6]))
        self.assertEqual(read_ppm(path), (2, 1, [(1, 2, 3), (4, 5, 6)]))

    def test_rejects_other_formats_and_truncation(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "header"):
            read_ppm(self.write(b"P3\n1 1\n255\n", b"0 0 0"))
        with self.assertRaisesRegex(RuntimeError, "truncated"):
            read_ppm(self.write(b"P6\n2 2\n255\n", bytes(11)))


class DifferenceTests(unittest.TestCase):
    def test_reports_the_first_differing_pixel(self) -> None:
        self.assertEqual(
            first_difference([(0, 0, 0)] * 4 + [(1, 1, 1)], [(0, 0, 0)] * 5, 2),
            "first difference at (0,2): got (1, 1, 1), expected (0, 0, 0)",
        )


if __name__ == "__main__":
    unittest.main()
