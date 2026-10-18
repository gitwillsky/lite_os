"""diagnose_scheduler_latency 的纯计算部分：任务集生成、理想基线、配对损失与解析。"""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import diagnose_scheduler_latency as tool  # noqa: E402


class JobLengthTests(unittest.TestCase):
    def test_lengths_match_the_guest_generator(self) -> None:
        # 这组值由 guest 的 `lengths` 模式核对过：工具每次运行前都会比较两边，漂移会让它直接失败。
        self.assertEqual(
            tool.job_lengths_ms(12345, 12),
            [20, 46, 62, 42, 81, 113, 63, 87, 20, 36, 64, 33],
        )

    def test_lengths_stay_inside_the_documented_range(self) -> None:
        for trial in range(25):
            for length in tool.job_lengths_ms(tool.trial_seed(trial), 16):
                self.assertTrue(20 <= length <= 119)

    def test_trial_seeds_are_distinct(self) -> None:
        self.assertEqual(len({tool.trial_seed(trial) for trial in range(25)}), 25)


class IdealMakespanTests(unittest.TestCase):
    def test_fewer_jobs_than_cpus_take_as_long_as_the_longest(self) -> None:
        self.assertAlmostEqual(tool.ideal_makespan_ms([50], 4), 50)
        self.assertAlmostEqual(tool.ideal_makespan_ms([10, 100], 2), 100)

    def test_equal_jobs_share_the_cpus_evenly(self) -> None:
        # 4 个 40 ms 的任务在 2 个 CPU 上：每个以 1/2 的速率运行，共 80 ms。
        self.assertAlmostEqual(tool.ideal_makespan_ms([40] * 4, 2), 80)
        # 3 个 60 ms 的任务在 2 个 CPU 上：总工作量 180 / 2 = 90 ms。
        self.assertAlmostEqual(tool.ideal_makespan_ms([60] * 3, 2), 90)

    def test_uneven_jobs_can_exceed_the_work_over_cpus_bound(self) -> None:
        # 总工作量 / CPU 数 = 120，最长任务 180。前 40 ms 四个任务各以 1/2 速率运行，之后最长任务
        # 还剩 160 ms 且只能占一个 CPU，共 200 ms：公平共享让最长任务早期变慢，所以超过两个下界。
        lengths = [20, 20, 20, 180]
        ideal = tool.ideal_makespan_ms(lengths, 2)
        self.assertGreater(ideal, max(sum(lengths) / 2, max(lengths)))
        self.assertAlmostEqual(ideal, 200)

    def test_it_never_beats_the_work_conserving_bound(self) -> None:
        for trial in range(25):
            lengths = tool.job_lengths_ms(tool.trial_seed(trial), 12)
            bound = max(sum(lengths) / 4, max(lengths))
            self.assertGreaterEqual(tool.ideal_makespan_ms(lengths, 4) + 1e-9, bound)


class PairedLossTests(unittest.TestCase):
    def test_a_trial_equal_to_the_ideal_has_no_loss(self) -> None:
        lengths = tool.job_lengths_ms(tool.trial_seed(0), 6)
        ideal_us = round(tool.ideal_makespan_ms(lengths, 2) * 1000)
        trial = tool.MakeTrial(2, 6, tool.trial_seed(0), ideal_us)
        self.assertAlmostEqual(tool.extra_loss_percent(trial), 0.0, places=2)

    def test_ten_percent_longer_than_the_ideal_is_a_ten_percent_loss(self) -> None:
        lengths = tool.job_lengths_ms(tool.trial_seed(3), 12)
        ideal_us = tool.ideal_makespan_ms(lengths, 4) * 1000
        trial = tool.MakeTrial(4, 12, tool.trial_seed(3), round(ideal_us * 1.1))
        self.assertAlmostEqual(tool.extra_loss_percent(trial), 10.0, places=1)


class ParsingTests(unittest.TestCase):
    def test_trials_are_extracted_from_noisy_serial_output(self) -> None:
        serial = (
            "boot noise\n"
            "SCHED_LAT_TRIAL make cpus=2 jobs=6 seed=12345 makespan_us=210500\n"
            "[CPU-0] [INFO] unrelated\n"
            "SCHED_LAT_TRIAL make cpus=2 jobs=6 seed=20264 makespan_us=198000\n"
            "SCHED_LAT_RESULT make cpus=2 jobs=6 trials=2\n"
        )
        self.assertEqual(
            tool.parse_make_trials(serial),
            [tool.MakeTrial(2, 6, 12345, 210500), tool.MakeTrial(2, 6, 20264, 198000)],
        )

    def test_the_lag_summary_is_recognized(self) -> None:
        line = "SCHED_LAT_RESULT lag n=40 timeouts=0 min_us=186 p50_us=5178 p90_us=9246 max_us=9956 mean_us=5158"
        match = tool.LAG_RE.search(line)
        self.assertIsNotNone(match)
        self.assertEqual(match.group(4), "5178")


class PercentileTests(unittest.TestCase):
    def test_nearest_rank(self) -> None:
        values = [1.0, 2.0, 3.0, 4.0, 5.0]
        self.assertEqual(tool.percentile(values, 50), 3.0)
        self.assertEqual(tool.percentile(values, 90), 5.0)
        self.assertEqual(tool.percentile(values, 1), 1.0)

    def test_order_does_not_matter(self) -> None:
        self.assertEqual(tool.percentile([5.0, 1.0, 3.0], 50), 3.0)


if __name__ == "__main__":
    unittest.main()
