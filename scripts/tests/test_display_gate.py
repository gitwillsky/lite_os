"""串口是字节流，任意分片不能损坏中文产品标记或 ANSI 边界。"""
import sys
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import display_gate


class DisplayConsoleTests(unittest.TestCase):
    def test_unicode_and_ansi_survive_single_byte_serial_chunks(self):
        output = SimpleNamespace(fileno=lambda: 7)
        console = display_gate.Console(SimpleNamespace(stdout=output))
        text = '\x1b[32mcompositor: window "文件资源管理器"\x1b[0m\n'
        chunks = [bytes([byte]) for byte in text.encode()] + [b'']
        with patch.object(display_gate.select, 'select', return_value=([output], [], [])), patch.object(display_gate.os, 'read', side_effect=chunks):
            console.wait_for(('compositor: window "文件资源管理器"',))
        self.assertNotIn('\ufffd', console.text)
        self.assertNotIn('\x1b[32m', console.text)


if __name__ == '__main__':
    unittest.main()
