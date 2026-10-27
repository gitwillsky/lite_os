"""verify_gpu 的判定逻辑：结果解析与各项阈值必须在应该红的时候红。"""

import contextlib
import io
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import verify_gpu  # noqa: E402

GOOD = {
    "width": 3008,
    "height": 1692,
    "frames": 600,
    "dropped": 0,
    "p50_us": 12_000,
    "p95_us": 14_000,
    "p99_us": 15_000,
}


def verdict(**overrides: int) -> tuple[int, str]:
    output = io.StringIO()
    with contextlib.redirect_stdout(output):
        code = verify_gpu.report({**GOOD, **overrides})
    return code, output.getvalue()


class VerifyGpuTests(unittest.TestCase):
    def test_parse_reads_the_probe_result_line(self) -> None:
        serial = (
            "boot noise\n"
            "OpenGL ES profile renderer: virgl\n"
            "EGL_WAYLAND_SHARING_READY\n"
            "drm-flip-probe: prime=ok exporter-close=ok implicit-sync=ok aliases=ok\n"
            "drm-flip-probe: mode=3008x1692 readback=ok frames=600 dropped=0 "
            "p50_us=12539 p95_us=14017 p99_us=14723\n"
            "drm-flip-probe: done\n"
        )
        self.assertEqual(
            verify_gpu.parse(serial),
            {**GOOD, "p50_us": 12_539, "p95_us": 14_017, "p99_us": 14_723},
        )

    def test_parse_surfaces_the_probe_failure_reason(self) -> None:
        serial = "drm-flip-probe: error: readback: pixel 0x00000000 is not the cleared color\n"
        with self.assertRaisesRegex(RuntimeError, "readback: pixel"):
            verify_gpu.parse(serial)

    def test_parse_rejects_output_without_a_result(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "no result line"):
            verify_gpu.parse("drm-flip-probe: done\n")

    def test_parse_rejects_a_result_that_skipped_the_readback_check(self) -> None:
        serial = (
            "drm-flip-probe: mode=3008x1692 readback=skipped frames=600 dropped=0 "
            "p50_us=1 p95_us=1 p99_us=1\n"
        )
        with self.assertRaisesRegex(RuntimeError, "no result line"):
            verify_gpu.parse(serial)

    def test_report_is_green_inside_every_limit(self) -> None:
        self.assertEqual(verdict()[0], 0)

    def test_limits_are_inclusive(self) -> None:
        code, _ = verdict(
            p95_us=verify_gpu.LIMIT_P95_US, p99_us=verify_gpu.LIMIT_P99_US
        )
        self.assertEqual(code, 0)

    def test_report_is_red_for_each_metric_over_its_limit(self) -> None:
        for override in (
            {"p95_us": verify_gpu.LIMIT_P95_US + 1},
            {"p99_us": verify_gpu.LIMIT_P99_US + 1},
            {"dropped": 1},
            {"frames": verify_gpu.MEASURED_FRAMES - 1},
        ):
            with self.subTest(override=override):
                code, output = verdict(**override)
                self.assertEqual(code, 1)
                self.assertIn("RED", output)


if __name__ == "__main__":
    unittest.main()
