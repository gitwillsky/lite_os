from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from verify_wayland import (  # noqa: E402
    DESKTOP_BACKGROUND, FIRST_WINDOW_ORIGIN, PROBE_LAST_COLOR, PROBE_SIZE, check_info, check_screen,
)

WIDTH, HEIGHT = 400, 300


def screen(window=PROBE_LAST_COLOR) -> list[tuple[int, int, int]]:
    pixels = [DESKTOP_BACKGROUND] * (WIDTH * HEIGHT)
    for y in range(FIRST_WINDOW_ORIGIN, FIRST_WINDOW_ORIGIN + PROBE_SIZE[1]):
        for x in range(FIRST_WINDOW_ORIGIN, FIRST_WINDOW_ORIGIN + PROBE_SIZE[0]):
            pixels[y * WIDTH + x] = window
    return pixels


INFO = """\
interface: 'wl_compositor',                              version:  6, name:  1
interface: 'wl_shm',                                     version:  1, name:  2
	formats (fourcc):
	         1 = 'XR24'
	         0 = 'AR24'
interface: 'xdg_wm_base',                                version:  3, name:  3
interface: 'wl_output',                                  version:  4, name:  4
	name: LITEOS-1
	x: 0, y: 0, scale: 1,
	make: 'LiteOS', model: 'Virtual Display',
	mode:
		width: 1280 px, height: 800 px, refresh: 60.000 Hz,
		flags: current preferred
"""


class ScreenTests(unittest.TestCase):
    def test_a_correct_composite_passes(self) -> None:
        check_screen(WIDTH, HEIGHT, screen())

    def test_each_kind_of_mistake_is_named(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "window center"):
            check_screen(WIDTH, HEIGHT, screen(window=(1, 2, 3)))
        shifted = screen()
        shifted[FIRST_WINDOW_ORIGIN * WIDTH + FIRST_WINDOW_ORIGIN - 1] = PROBE_LAST_COLOR
        with self.assertRaisesRegex(RuntimeError, "left of window"):
            check_screen(WIDTH, HEIGHT, shifted)
        empty = [DESKTOP_BACKGROUND] * (WIDTH * HEIGHT)
        with self.assertRaisesRegex(RuntimeError, "window top-left"):
            check_screen(WIDTH, HEIGHT, empty)


class InfoTests(unittest.TestCase):
    def test_upstream_output_is_accepted(self) -> None:
        check_info(INFO)

    def test_a_missing_global_or_wrong_mode_is_reported(self) -> None:
        for broken in (
            INFO.replace("'xdg_wm_base'", "'xdg_other'"),
            INFO.replace("version:  6", "version:  5"),
            INFO.replace("1280 px", "1024 px"),
            INFO.replace("'AR24'", "'AB24'"),
        ):
            with self.assertRaises(RuntimeError):
                check_info(broken)


if __name__ == "__main__":
    unittest.main()
