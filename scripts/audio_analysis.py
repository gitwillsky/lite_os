#!/usr/bin/env python3
"""分析 QEMU WAV backend 产生的固定 48 kHz stereo PCM。"""

from __future__ import annotations

import math
import struct
import wave
from dataclasses import dataclass
from pathlib import Path

SAMPLE_RATE = 48_000
CHANNELS = 2
SAMPLE_WIDTH = 2


@dataclass(frozen=True)
class WavSignal:
    """一份已经验证格式的 QEMU WAV 信号。

    Attributes:
        frames: 归一化到 `[-1, 1)` 的 interleaved stereo frame。
        sample_rate: WAV header 中的采样率；当前固定为 48 kHz。
    """

    frames: tuple[tuple[float, float], ...]
    sample_rate: int

    @property
    def duration_seconds(self) -> float:
        """返回 WAV 的精确 frame duration。"""
        return len(self.frames) / self.sample_rate

    def peak(self) -> float:
        """返回双声道绝对 sample peak。"""
        return max((abs(sample) for frame in self.frames for sample in frame), default=0.0)

    def rms(self, channel: int | None = None) -> float:
        """返回全部 sample 或指定声道的 RMS。

        Args:
            channel: `None` 合并双声道；`0`/`1` 只分析指定声道。

        Returns:
            归一化 amplitude RMS。

        Raises:
            ValueError: channel 不是 `None`、0 或 1。
        """
        if channel not in (None, 0, 1):
            raise ValueError("channel must be None, 0, or 1")
        samples = (
            (sample for frame in self.frames for sample in frame)
            if channel is None
            else (frame[channel] for frame in self.frames)
        )
        total = 0.0
        count = 0
        for sample in samples:
            total += sample * sample
            count += 1
        return math.sqrt(total / count) if count else 0.0

    def tone_amplitude(
        self,
        frequency: float,
        start_frame: int = 0,
        frame_count: int | None = None,
    ) -> float:
        """用双声道平均信号的正交投影测量一个固定频率。

        Args:
            frequency: 目标频率 Hz，必须在 `(0, Nyquist)`。
            start_frame: 分析窗口的首 frame。
            frame_count: 分析 frame 数；None 使用剩余全部 frame。

        Returns:
            正弦振幅估计；完整周期窗口中与输入 amplitude 一致。

        Raises:
            ValueError: 频率或窗口超出信号范围。
        """
        if not 0.0 < frequency < self.sample_rate / 2:
            raise ValueError("frequency must be below Nyquist")
        if start_frame < 0 or start_frame > len(self.frames):
            raise ValueError("start_frame is outside the signal")
        count = len(self.frames) - start_frame if frame_count is None else frame_count
        if count <= 0 or start_frame + count > len(self.frames):
            raise ValueError("analysis window is outside the signal")
        angular = 2.0 * math.pi * frequency / self.sample_rate
        sine = 0.0
        cosine = 0.0
        for index, frame in enumerate(self.frames[start_frame : start_frame + count]):
            sample = (frame[0] + frame[1]) * 0.5
            phase = angular * index
            sine += sample * math.sin(phase)
            cosine += sample * math.cos(phase)
        return 2.0 * math.hypot(sine, cosine) / count


def read_qemu_wav(path: Path) -> WavSignal:
    """读取并验证 runtime gate 的 QEMU WAV 输出。

    Args:
        path: QEMU `-audiodev wav` 产生的文件。

    Returns:
        可执行确定性分析的归一化 stereo signal。

    Raises:
        RuntimeError: 文件缺失、WAV layout 不符合 48 kHz stereo S16 contract，
            或 payload 不是完整 frame。
    """
    if not path.is_file():
        raise RuntimeError(f"QEMU audio output is missing: {path}")
    try:
        with wave.open(str(path), "rb") as source:
            layout = (
                source.getnchannels(),
                source.getsampwidth(),
                source.getframerate(),
                source.getcomptype(),
            )
            expected = (CHANNELS, SAMPLE_WIDTH, SAMPLE_RATE, "NONE")
            if layout != expected:
                raise RuntimeError(
                    f"QEMU WAV layout is {layout!r}, expected {expected!r}"
                )
            payload = source.readframes(source.getnframes())
    except (EOFError, wave.Error) as error:
        raise RuntimeError(f"invalid QEMU WAV output: {error}") from error
    frame_bytes = CHANNELS * SAMPLE_WIDTH
    if len(payload) % frame_bytes != 0:
        raise RuntimeError("QEMU WAV payload ends inside one stereo frame")
    samples = struct.iter_unpack("<hh", payload)
    return WavSignal(
        tuple((left / 32768.0, right / 32768.0) for left, right in samples),
        SAMPLE_RATE,
    )
