"""verify_scheduler_fairness 的 guest 命令与判定公式。"""

import re
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import verify_scheduler_fairness as gate  # noqa: E402


def verdict(minimum: int, maximum: int) -> int:
    """Evaluates the guest command's own verdict expression with the host POSIX shell."""
    command = gate.fairness_command(2, 3).decode()
    expression = re.search(r"verdict=\$\(\(.*?\)\)", command)
    assert expression is not None
    result = subprocess.run(
        ["sh", "-c", f"min={minimum}; max={maximum}; {expression.group(0)}; echo $verdict"],
        capture_output=True,
        text=True,
        check=True,
    )
    return int(result.stdout)


class SchedulerFairnessGateTests(unittest.TestCase):
    def test_typed_command_never_contains_a_verdict_marker(self) -> None:
        # shell 会把键入的命令回显到串口；标记若出现在命令里，门禁会被回显误判通过。
        for cpus, tasks in gate.CONFIGURATIONS:
            command = gate.fairness_command(cpus, tasks).decode()
            for value in (0, 1):
                self.assertNotIn(gate.marker(cpus, tasks, value), command)

    def test_markers_are_distinct_per_configuration_and_verdict(self) -> None:
        markers = {
            gate.marker(cpus, tasks, value)
            for cpus, tasks in gate.CONFIGURATIONS
            for value in (0, 1)
        }
        self.assertEqual(len(markers), 2 * len(gate.CONFIGURATIONS))

    def test_the_locked_distribution_fails_and_a_fair_one_passes(self) -> None:
        # 实测：修复前 2 CPU / 3 任务为 307..614，修复后为 404..422。
        self.assertEqual(verdict(307, 614), 0)
        self.assertEqual(verdict(404, 422), 1)

    def test_the_threshold_is_inclusive(self) -> None:
        self.assertEqual(verdict(gate.MIN_FAIR_PERCENT, 100), 1)
        self.assertEqual(verdict(gate.MIN_FAIR_PERCENT - 1, 100), 0)

    def test_every_configuration_has_more_tasks_than_cpus(self) -> None:
        for cpus, tasks in gate.CONFIGURATIONS:
            self.assertEqual(tasks, cpus + 1)


if __name__ == "__main__":
    unittest.main()
