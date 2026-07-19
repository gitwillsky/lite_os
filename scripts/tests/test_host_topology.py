from __future__ import annotations

import sys
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from host_topology import default_guest_cpu_count  # noqa: E402


class HostTopologyTests(unittest.TestCase):
    def test_guest_keeps_one_physical_cpu_for_the_host(self) -> None:
        self.assertEqual(default_guest_cpu_count(12), 11)

    def test_single_cpu_host_still_gives_guest_one_cpu(self) -> None:
        self.assertEqual(default_guest_cpu_count(1), 1)

    def test_invalid_host_count_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "must be positive"):
            default_guest_cpu_count(0)


if __name__ == "__main__":
    unittest.main()
