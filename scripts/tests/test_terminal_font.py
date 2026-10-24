"""保留的终端字库不能依赖已退休的 UI 构建器或可选渲染包。"""
import subprocess
import sys
import unittest
from pathlib import Path


class TerminalFontTests(unittest.TestCase):
    def test_checked_atlas_verifies_without_optional_host_packages(self):
        root = Path(__file__).resolve().parents[2]
        result = subprocess.run(
            [sys.executable, "-S", "scripts/generate_terminal_font.py", "--verify"],
            cwd=root, check=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        self.assertIn("Chinese samples nonempty", result.stdout)


if __name__ == "__main__":
    unittest.main()
