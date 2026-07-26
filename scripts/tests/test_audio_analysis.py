from __future__ import annotations

import math
import struct
import tempfile
import unittest
import wave
from pathlib import Path

from scripts.audio_analysis import read_qemu_wav


class AudioAnalysisTests(unittest.TestCase):
    def write_tone(
        self,
        path: Path,
        frequency: float = 1_000.0,
        amplitude: float = 0.25,
        frames: int = 48_000,
    ) -> None:
        payload = bytearray()
        for index in range(frames):
            sample = round(
                math.sin(2.0 * math.pi * frequency * index / 48_000)
                * amplitude
                * 32767
            )
            payload.extend(struct.pack("<hh", sample, sample))
        with wave.open(str(path), "wb") as output:
            output.setnchannels(2)
            output.setsampwidth(2)
            output.setframerate(48_000)
            output.writeframes(payload)

    def test_reads_fixed_layout_and_measures_tone(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "tone.wav"
            self.write_tone(path)
            signal = read_qemu_wav(path)

        self.assertEqual(signal.duration_seconds, 1.0)
        self.assertAlmostEqual(signal.peak(), 0.25, places=3)
        self.assertAlmostEqual(signal.rms(), 0.25 / math.sqrt(2.0), places=3)
        self.assertAlmostEqual(signal.tone_amplitude(1_000.0), 0.25, places=3)
        self.assertLess(signal.tone_amplitude(1_300.0), 0.001)

    def test_rejects_wrong_sample_rate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "wrong.wav"
            self.write_tone(path, frames=44_100)
            with wave.open(str(path), "rb") as source:
                payload = source.readframes(source.getnframes())
            with wave.open(str(path), "wb") as output:
                output.setnchannels(2)
                output.setsampwidth(2)
                output.setframerate(44_100)
                output.writeframes(payload)
            with self.assertRaisesRegex(RuntimeError, "expected"):
                read_qemu_wav(path)

    def test_rejects_missing_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, "missing"):
                read_qemu_wav(Path(directory) / "missing.wav")


if __name__ == "__main__":
    unittest.main()
