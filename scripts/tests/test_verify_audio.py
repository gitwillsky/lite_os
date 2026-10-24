from __future__ import annotations

import contextlib
import io
import math
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from audio_analysis import WavSignal
from verify_audio import analyze


class AudioOutputTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        tones = [[math.sin(2 * math.pi * frequency * i / 48_000) for i in range(48_000)]
                 for frequency in range(440, 1320, 110)]
        # 独立列出验收向量，避免用 verifier 的增益公式生成它自己的 oracle。
        levels = [
            ([0.16] + [0] * 7, [0.16] + [0] * 7),
            ([0.04] * 8, [0.04] * 8),
            ([0.01] + [0.04] * 7, [0.04] * 8),
            ([0.0025] + [0.01] * 7, [0.0225] * 8),
            ([0] * 8, [0.0225] * 8),
        ]
        frames = []
        for left, right in levels:
            frames.extend((sum(tone[i] * gain for tone, gain in zip(tones, left)),
                           sum(tone[i] * gain for tone, gain in zip(tones, right)))
                          for i in range(48_000))
        frames.extend([(32767 / 32768, 32767 / 32768)] * 48_000)
        cls.frames = tuple(frames)

    def verify(self, frames) -> None:
        with contextlib.redirect_stdout(io.StringIO()):
            analyze(WavSignal(tuple(frames), 48_000))

    def test_all_tones_gain_mute_and_clip(self) -> None:
        self.verify(self.frames)

    def test_missing_stream_is_rejected(self) -> None:
        frames = list(self.frames)
        for i in range(48_000, 96_000):
            missing = 0.04 * math.sin(2 * math.pi * 1210 * i / 48_000)
            frames[i] = tuple(sample - missing for sample in frames[i])
        with self.assertRaisesRegex(RuntimeError, "stream=7"):
            self.verify(frames)

    def test_wrong_channel_gain_is_rejected(self) -> None:
        frames = list(self.frames)
        frames[144_000:192_000] = [(left * 2, right) for left, right in frames[144_000:192_000]]
        with self.assertRaisesRegex(RuntimeError, "phase=3.*channel=0"):
            self.verify(frames)

    def test_mute_leak_is_rejected(self) -> None:
        frames = list(self.frames)
        frames[192_000:240_000] = [(0.001, right) for _, right in frames[192_000:240_000]]
        with self.assertRaisesRegex(RuntimeError, "muted left"):
            self.verify(frames)

    def test_wrapped_overload_is_rejected(self) -> None:
        frames = self.frames[:240_000] + ((-0.5, -0.5),) * 48_000
        with self.assertRaisesRegex(RuntimeError, "hard clip"):
            self.verify(frames)
