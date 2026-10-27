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

    def test_phase_budget_does_not_change_the_default_or_hide_fatal_output(self):
        output = SimpleNamespace(fileno=lambda: 7)
        process = SimpleNamespace(stdout=output)
        with patch.object(display_gate.time, 'monotonic', side_effect=[0, 91]):
            with self.assertRaisesRegex(RuntimeError, 'timed out'):
                display_gate.Console(process).wait_for(('ready',))
        console = display_gate.Console(process)
        with patch.object(display_gate.time, 'monotonic', side_effect=[0, 91]), patch.object(display_gate.select, 'select', return_value=([output], [], [])), patch.object(display_gate.os, 'read', return_value=b'ready'):
            console.wait_for(('ready',), timeout_seconds=300)
        with patch.object(display_gate.time, 'monotonic', side_effect=[0, 1]), patch.object(display_gate.select, 'select', return_value=([output], [], [])), patch.object(display_gate.os, 'read', return_value=b'[ERROR]'):
            with self.assertRaisesRegex(RuntimeError, 'fatal marker'):
                display_gate.Console(process).wait_for(('ready',), timeout_seconds=300)


if __name__ == '__main__':
    unittest.main()
