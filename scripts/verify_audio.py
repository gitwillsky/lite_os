#!/usr/bin/env python3
"""验证 AArch64/HVF 的 production Music Player 到 VirtIO Sound 完整链路。"""

from __future__ import annotations

import argparse
import math
import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path

from audio_analysis import WavSignal, read_qemu_wav
from build_cache import publish_runtime_gate, runtime_gate_hit, runtime_gate_payload
from build_target import acceleration_from_environment, target_from_environment
from ext2_image import recover_ext2_journal, run_debugfs
from qemu_gate import ANSI, QmpClient, _qemu_command, terminate

ROOT = Path(__file__).resolve().parent.parent
FIXTURE_DIRECTORY = ROOT / "scripts" / "fixtures" / "audio"
DISPLAY_WIDTH = 1504
DISPLAY_HEIGHT = 846
RECIPE_VERSION = 5
APP_ORIGIN = (155, 117)
APP_CASCADE = (28, 24)
QUEUE_ROW_POINT = (580, 112)
QUEUE_ROW_HEIGHT = 48
QUEUE_FIRST_SCROLL_ROWS = 5
QUEUE_BOTTOM_OFFSET = 402
REPEAT_BUTTON_POINT = (543, 20)
PLAY_BUTTON_POINT = (224, 330)
SEEK_POINT = (156, 294)
ELEMENT_MUTE_POINT = (35, 427)
ELEMENT_VOLUME_X = {50: 143, 80: 188}
MASTER_SPEAKER_POINT = (1429, 831)
MASTER_MUTE_POINT = (1407, 795)
MASTER_SCALE_Y = 749
MASTER_VOLUME_X = {30: 1380, 70: 1436, 100: 1478}
S16_POSITIVE_MAX = 32_767 / 32_768
POINTER_HOVER_SETTLE_SECONDS = 0.1

# Guest names make the production directory sort byte-for-byte deterministic.
# Without the numeric prefix, locale-dependent `localeCompare` ordering could
# make the gate click a different codec while still observing a valid marker.
FIXTURES = (
    ("tone-aac.m4a", "01-tone-aac.m4a"),
    ("tone-alac.m4a", "02-tone-alac.m4a"),
    ("tone.aiff", "03-tone.aiff"),
    ("tone.caf", "04-tone.caf"),
    ("tone.flac", "05-tone.flac"),
    ("tone.mka", "06-tone.mka"),
    ("tone.mp1", "07-tone.mp1"),
    ("tone.mp2", "08-tone.mp2"),
    ("tone.mp3", "09-tone.mp3"),
    ("tone.mp4", "10-tone.mp4"),
    ("tone.ogg", "11-tone.ogg"),
    ("tone.wav", "12-tone.wav"),
    ("tone.webm", "13-tone.webm"),
)
LIMITER_FIXTURE = ("limiter-dc.wav", "99-limiter-dc.wav")

METRICS_RE = re.compile(
    r"audio-service: metrics periods=(\d+) xrun=(\d+) "
    r"steady_allocations=(\d+) idle_periodic_wakes=(\d+) "
    r"mix_p99_us=(\d+) limiter_activations=(\d+) "
    r"limiter_max_reduction=([0-9.]+)"
)
SOURCE_OPENED_RE = re.compile(r"LITE_AUDIO source-opened id=\d+ file=([^\s]+)")
LOOP_STATE_RE = re.compile(r"LITE_AUDIO loop id=(\d+) enabled=(true|false)")
WORKER_ALLOCATIONS_RE = re.compile(
    r"LITE_AUDIO worker-allocation id=\d+ warmup_epoch=\d+ steady_allocations=(\d+)"
)
STREAM_LIFECYCLE_RE = re.compile(r"audio-service: stream (start|close) id=(\d+)")
FATAL_MARKERS = (
    "panicked at",
    "[ERROR]",
    "[Audio] ALSA playback XRUN",
    "audio-service: unavailable",
    "LITE_AUDIO event=error",
)


@dataclass(frozen=True)
class AudioGateResult:
    """一次完整 runtime run 的可缓存裁决结果。"""

    files_played: int
    concurrent_streams: int
    duration_seconds: float
    rms: float
    tone_440: float
    tone_660: float
    periods: int
    xrun: int
    steady_allocations: int
    idle_periodic_wakes: int
    mix_p99_us: int
    limiter_activations: int
    limiter_max_reduction: float


class SerialCapture:
    """持续 drain QEMU 串口并提供单调 marker 等待。"""

    def __init__(self, process: subprocess.Popen[bytes]) -> None:
        if process.stdout is None:
            raise RuntimeError("QEMU stdout pipe is unavailable")
        self._process = process
        self._stream = process.stdout
        self._output = bytearray()
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._reader, daemon=True)
        self._thread.start()

    def _reader(self) -> None:
        while not self._stop.is_set():
            ready, _, _ = select.select([self._stream], [], [], 0.1)
            if not ready:
                if self._process.poll() is not None:
                    return
                continue
            chunk = os.read(self._stream.fileno(), 16 * 1024)
            if not chunk:
                return
            with self._lock:
                self._output.extend(chunk)

    def text(self) -> str:
        """返回已经去除 ANSI escape 的完整串口快照。"""
        with self._lock:
            return ANSI.sub("", bytes(self._output).decode(errors="replace"))

    def count(self, marker: str) -> int:
        """返回 marker 当前出现次数。"""
        return self.text().count(marker)

    def wait_new(self, marker: str, previous: int, timeout_seconds: float) -> None:
        """等待 marker 次数严格超过 previous，并在 guest fatal 时立即失败。"""
        deadline = time.monotonic() + timeout_seconds
        while time.monotonic() < deadline:
            text = self.text()
            fatal = next((candidate for candidate in FATAL_MARKERS if candidate in text), None)
            if fatal is not None:
                raise RuntimeError(
                    f"audio guest reached fatal marker {fatal!r}\n"
                    f"--- output tail ---\n{serial_tail(text)}"
                )
            if text.count(marker) > previous:
                return
            if self._process.poll() is not None:
                break
            time.sleep(0.05)
        raise RuntimeError(
            f"audio guest missed marker {marker!r}\n"
            f"--- output tail ---\n{serial_tail(self.text())}"
        )

    def wait_all(self, markers: tuple[str, ...], timeout_seconds: float) -> None:
        """等待一组启动 marker 全部出现。"""
        deadline = time.monotonic() + timeout_seconds
        while time.monotonic() < deadline:
            text = self.text()
            fatal = next((candidate for candidate in FATAL_MARKERS if candidate in text), None)
            if fatal is not None:
                raise RuntimeError(
                    f"audio guest reached fatal marker {fatal!r}\n"
                    f"--- output tail ---\n{serial_tail(text)}"
                )
            if all(marker in text for marker in markers):
                return
            if self._process.poll() is not None:
                break
            time.sleep(0.05)
        text = self.text()
        missing = [marker for marker in markers if marker not in text]
        raise RuntimeError(
            f"audio guest missed startup markers: {missing!r}\n"
            f"--- output tail ---\n{serial_tail(text)}"
        )

    def close(self) -> None:
        """停止 reader，并保留已经 drain 的内容供最终裁决。"""
        self._stop.set()
        self._thread.join(timeout=2)


def serial_tail(text: str, lines: int = 50) -> str:
    """返回适合失败输出的串口尾部。"""
    return "\n".join(text.splitlines()[-lines:])


def inject_fixtures(image: Path) -> None:
    """用 debugfs 把固定 codec 矩阵写入 disposable `/root/Music`。

    Args:
        image: 当前没有 QEMU 使用的私有 rootfs 副本。

    Returns:
        13 个 fixture 全部存在于 guest 目录后返回。

    Raises:
        RuntimeError: fixture 缺失、journal 无法恢复或 debugfs 写入失败。
    """
    injected = (*FIXTURES, LIMITER_FIXTURE)
    missing = [
        source for source, _ in injected if not (FIXTURE_DIRECTORY / source).is_file()
    ]
    if missing:
        raise RuntimeError(f"audio fixtures are missing: {missing!r}")
    recover_ext2_journal(image)
    listing = run_debugfs(image, "stat /root/Music")
    if "File not found" in listing:
        raise RuntimeError("rootfs does not contain /root/Music")
    for source, guest_name in injected:
        run_debugfs(
            image,
            f"write {FIXTURE_DIRECTORY / source} /root/Music/{guest_name}",
            writable=True,
        )
    final_listing = run_debugfs(image, "ls -l /root/Music")
    absent = [guest for _, guest in injected if guest not in final_listing]
    if absent:
        raise RuntimeError(f"debugfs did not publish audio fixtures: {absent!r}")


def click(qmp: QmpClient, x: float, y: float) -> None:
    """通过 virtio-tablet 在一个 logical pixel 坐标执行真实左键点击。"""
    qmp.move_abs(x / DISPLAY_WIDTH, y / DISPLAY_HEIGHT)
    # PointerEnter 会让 production React button 提交 hover scene。没有这个
    # 间隔，紧随其后的 button-down 偶发仍路由到旧 hit tree，Mute/Repeat
    # 点击便会在串口看似无故消失。
    time.sleep(POINTER_HOVER_SETTLE_SECONDS)
    qmp.button("left", True)
    qmp.button("left", False)


def double_click(qmp: QmpClient, x: float, y: float) -> None:
    """通过两个真实 click 触发 production React `onDoubleClick`。"""
    for _ in range(2):
        click(qmp, x, y)
        time.sleep(0.08)


def scroll_queue(qmp: QmpClient, app_x: float, app_y: float, steps: int) -> None:
    """滚动 production Up Next 容器，使后续曲目进入可点击 viewport。"""
    qmp.move_abs(
        (app_x + QUEUE_ROW_POINT[0]) / DISPLAY_WIDTH,
        (app_y + 250) / DISPLAY_HEIGHT,
    )
    time.sleep(POINTER_HOVER_SETTLE_SECONDS)
    for _ in range(steps):
        qmp.button("wheel-down", True)
        qmp.button("wheel-down", False)
    time.sleep(0.1)


def toggle_master_popup(qmp: QmpClient) -> None:
    """切换 production taskbar speaker popup 并等待新 hit geometry 发布。"""
    click(qmp, *MASTER_SPEAKER_POINT)
    time.sleep(0.1)


def wav_frame_count(path: Path) -> int:
    """返回 QEMU WAV 当前已写 payload frame 数；文件未创建时为零。"""
    try:
        size = path.stat().st_size
    except FileNotFoundError:
        return 0
    return max(0, size - 44) // 4


def wait_for_wav_growth(path: Path, minimum_frames: int, timeout_seconds: float = 3.0) -> int:
    """等待 WAV payload 达到 minimum_frames，防止 host sleep 冒充播放。"""
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        frames = wav_frame_count(path)
        if frames >= minimum_frames:
            return frames
        time.sleep(0.02)
    raise RuntimeError(
        f"QEMU WAV stopped at {wav_frame_count(path)} frames, expected {minimum_frames}"
    )


def assert_idle_audio(path: Path, capture: SerialCapture) -> None:
    """裁决空桌面不启动 device，也不产生非静音 WAV payload。"""
    if "audio-service: device start" in capture.text():
        raise RuntimeError("audio device started before the first physical play action")
    before = wav_frame_count(path)
    time.sleep(0.5)
    after = wav_frame_count(path)
    if after != before:
        raise RuntimeError(f"idle audio WAV grew before playback: {before} -> {after} frames")
    if path.is_file() and any(path.read_bytes()[44:]):
        raise RuntimeError("idle audio WAV contains non-silent samples before playback")


def signal_rms(signal: WavSignal, start: int, end: int) -> float:
    """返回一个有效 WAV frame 区间的双声道 RMS。"""
    bounded_start = min(max(0, start), len(signal.frames))
    bounded_end = min(max(bounded_start, end), len(signal.frames))
    samples = [
        sample
        for frame in signal.frames[bounded_start:bounded_end]
        for sample in frame
    ]
    if not samples:
        return 0.0
    return (sum(sample * sample for sample in samples) / len(samples)) ** 0.5


def validate_peak_windows(signal: WavSignal, limiter_workload_start: int) -> None:
    """区分透明播放区间与有意触发 limiter 的满幅压力区间。"""
    if not 0 < limiter_workload_start < len(signal.frames):
        raise RuntimeError("limiter workload boundary is outside the WAV payload")
    pre_stress_peak = max(
        abs(sample)
        for frame in signal.frames[:limiter_workload_start]
        for sample in frame
    )
    stress_peak = max(
        abs(sample)
        for frame in signal.frames[limiter_workload_start:]
        for sample in frame
    )
    overall_peak = max(pre_stress_peak, stress_peak)
    if pre_stress_peak >= 0.999:
        raise RuntimeError(
            f"codec/control output clips before limiter stress at peak={pre_stress_peak:.6f}"
        )
    if overall_peak > S16_POSITIVE_MAX:
        raise RuntimeError(
            f"audio output exceeds the positive S16 sample ceiling: peak={overall_peak:.6f}"
        )
    if stress_peak < 0.99:
        raise RuntimeError(
            f"limiter stress did not approach the S16 ceiling: peak={stress_peak:.6f}"
        )


def channel_tone_amplitude(
    signal: WavSignal,
    channel: int,
    frequency: float,
) -> float:
    """测量一个物理声道上的确定性正弦振幅。"""
    if channel not in (0, 1):
        raise ValueError("channel must be 0 or 1")
    angular = 2.0 * math.pi * frequency / signal.sample_rate
    sine = 0.0
    cosine = 0.0
    for index, frame in enumerate(signal.frames):
        sample = frame[channel]
        sine += sample * math.sin(angular * index)
        cosine += sample * math.cos(angular * index)
    return 2.0 * math.hypot(sine, cosine) / len(signal.frames)


def parse_metrics(text: str) -> dict[str, int | float]:
    """返回最后一组 service metrics，并拒绝任何历史非零 invariant。"""
    matches = [
        (
            int(match.group(1)),
            int(match.group(2)),
            int(match.group(3)),
            int(match.group(4)),
            int(match.group(5)),
            int(match.group(6)),
            float(match.group(7)),
        )
        for match in METRICS_RE.finditer(text)
    ]
    if not matches:
        raise RuntimeError("audio service emitted no complete metrics window")
    for periods, xrun, allocations, idle_wakes, mix_p99, _, _ in matches:
        if xrun != 0 or allocations != 0 or idle_wakes != 0 or mix_p99 > 2670:
            raise RuntimeError(
                "audio metrics violated the runtime contract: "
                f"periods={periods} xrun={xrun} steady_allocations={allocations} "
                f"idle_periodic_wakes={idle_wakes} mix_p99_us={mix_p99}"
            )
    periods, xrun, allocations, idle_wakes, mix_p99, activations, reduction = matches[-1]
    return {
        "periods": periods,
        "xrun": xrun,
        "steady_allocations": allocations,
        "idle_periodic_wakes": idle_wakes,
        "mix_p99_us": mix_p99,
        "limiter_activations": activations,
        "limiter_max_reduction": reduction,
    }


def live_stream_ids(text: str) -> set[int]:
    """按串口顺序投影尚未 close 的已启动 service stream identity。"""
    live: set[int] = set()
    for operation, identity in STREAM_LIFECYCLE_RE.findall(text):
        stream_id = int(identity)
        if operation == "start":
            live.add(stream_id)
        else:
            live.discard(stream_id)
    return live


def assert_qemu_wav_finalized(path: Path) -> None:
    """验证 QEMU 已按实际文件长度回填 RIFF 与 data chunk 长度。"""
    size = path.stat().st_size
    with path.open("rb") as stream:
        header = stream.read(44)
    if (
        len(header) != 44
        or header[:4] != b"RIFF"
        or header[8:12] != b"WAVE"
        or header[36:40] != b"data"
    ):
        raise RuntimeError("QEMU did not finalize a canonical PCM WAV header")
    riff_size = int.from_bytes(header[4:8], "little")
    data_size = int.from_bytes(header[40:44], "little")
    if data_size == 0 or riff_size != size - 8 or data_size != size - 44:
        raise RuntimeError(
            "QEMU WAV header lengths do not match captured payload: "
            f"file={size} riff={riff_size} data={data_size}"
        )


def validate_signal(
    output: Path,
    limiter_workload_start: int,
    element_quiet_window: tuple[int, int],
    element_audible_window: tuple[int, int],
    master_quiet_window: tuple[int, int],
    master_low_window: tuple[int, int],
    master_reference_window: tuple[int, int],
) -> tuple[WavSignal, float, float]:
    """验证 layout、目标频率，以及 element/master 控件的物理输出。"""
    signal = read_qemu_wav(output)
    if signal.duration_seconds < 2.0:
        raise RuntimeError(
            f"audio output is too short for the production workload: {signal.duration_seconds:.3f}s"
        )
    if signal.rms() < 0.005 or signal.peak() <= 0.01:
        raise RuntimeError("audio output is silent")
    validate_peak_windows(signal, limiter_workload_start)
    reference_start, reference_end = element_audible_window
    if not 0 <= reference_start < reference_end <= len(signal.frames):
        raise RuntimeError("UA audible reference window is outside the WAV payload")
    reference = WavSignal(
        signal.frames[reference_start:reference_end],
        signal.sample_rate,
    )
    tone_440 = reference.tone_amplitude(440.0)
    tone_660 = reference.tone_amplitude(660.0)
    if tone_440 < 0.005 or tone_660 < 0.005:
        raise RuntimeError(
            f"decoded tone frequencies are absent: 440Hz={tone_440:.6f}, "
            f"660Hz={tone_660:.6f}"
        )
    left_440 = channel_tone_amplitude(reference, 0, 440.0)
    left_660 = channel_tone_amplitude(reference, 0, 660.0)
    right_440 = channel_tone_amplitude(reference, 1, 440.0)
    right_660 = channel_tone_amplitude(reference, 1, 660.0)
    if left_440 < 0.005 or right_660 < 0.005:
        raise RuntimeError(
            f"stereo channel tones are absent: left440={left_440:.6f}, "
            f"right660={right_660:.6f}"
        )
    if left_660 > left_440 * 0.35 or right_440 > right_660 * 0.35:
        raise RuntimeError(
            "stereo channel identity collapsed or crossed: "
            f"left440={left_440:.6f} left660={left_660:.6f} "
            f"right440={right_440:.6f} right660={right_660:.6f}"
        )
    element_muted_rms = signal_rms(signal, *element_quiet_window)
    element_reference_rms = signal_rms(signal, *element_audible_window)
    if element_reference_rms < 0.005:
        raise RuntimeError("UA controls produced no audible reference interval")
    if element_muted_rms > max(0.0005, element_reference_rms * 0.05):
        raise RuntimeError(
            f"UA mute did not silence device output: muted={element_muted_rms:.6f}, "
            f"reference={element_reference_rms:.6f}"
        )
    master_muted_rms = signal_rms(signal, *master_quiet_window)
    master_low_rms = signal_rms(signal, *master_low_window)
    master_reference_rms = signal_rms(signal, *master_reference_window)
    if master_reference_rms < 0.005 or master_low_rms < 0.0005:
        raise RuntimeError(
            "desktop master volume produced no audible comparison intervals: "
            f"low={master_low_rms:.6f} reference={master_reference_rms:.6f}"
        )
    if master_muted_rms > max(0.0005, master_reference_rms * 0.05):
        raise RuntimeError(
            "desktop master mute did not silence device output: "
            f"muted={master_muted_rms:.6f} reference={master_reference_rms:.6f}"
        )
    if not master_low_rms < master_reference_rms * 0.2:
        raise RuntimeError(
            "desktop master 30% step did not follow the cubic gain curve relative to 70%: "
            f"low={master_low_rms:.6f} reference={master_reference_rms:.6f}"
        )
    return signal, tone_440, tone_660


def run_audio_gate(image: Path, timeout_seconds: int = 120) -> AudioGateResult:
    """执行 production Music Player 的完整 AArch64 audio runtime gate。

    Args:
        image: 不可原地修改的 rootfs baseline。
        timeout_seconds: boot、交互和输出的总 host liveness 上限。

    Returns:
        已通过 codec、controls、metrics 与 WAV assertions 的结果。

    Raises:
        RuntimeError: QEMU、guest、UI、codec、device 或性能契约任一失败。
    """
    private = tempfile.TemporaryDirectory(prefix="liteos-audio-gate-")
    private_root = Path(private.name)
    private_image = private_root / image.name
    qmp_socket = private_root / "qmp.sock"
    audio_output = private_root / "audio.wav"
    shutil.copyfile(image, private_image)
    inject_fixtures(private_image)
    process = subprocess.Popen(
        _qemu_command(
            private_image,
            1,
            interactive_devices=True,
            qmp_socket=qmp_socket,
            audio_output=audio_output,
        ),
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    capture = SerialCapture(process)
    qmp: QmpClient | None = None
    element_quiet_window = (0, 0)
    element_audible_window = (0, 0)
    master_quiet_window = (0, 0)
    master_low_window = (0, 0)
    master_reference_window = (0, 0)
    live_streams: set[int] = set()
    limiter_workload_start = 0
    final_text = ""
    deadline = time.monotonic() + timeout_seconds

    def retain_failure_artifacts() -> None:
        if os.environ.get("LITEOS_KEEP_FAILED_AUDIO") != "1":
            return
        failure = (
            ROOT / "target" / "audio-gate-failures" / f"{os.getpid()}-{time.time_ns()}"
        )
        failure.mkdir(parents=True)
        (failure / "serial.log").write_text(final_text)
        shutil.copyfile(private_image, failure / private_image.name)
        if audio_output.is_file():
            shutil.copyfile(audio_output, failure / audio_output.name)
        print(
            f"audio runtime failure artifacts retained in {failure}",
            file=sys.stderr,
        )

    try:
        # 1. Empty desktop owns the idle contract. A configured ALSA device is
        #    allowed, but START and WAV frames are forbidden before physical play.
        capture.wait_all(
            (
                "[Audio] VirtIO Sound capability ready",
                "audio-service: ready",
                "compositor: desktop first scene presented",
                "lite-ui: desktop ready",
            ),
            min(45.0, timeout_seconds),
        )
        assert_idle_audio(audio_output, capture)
        qmp = QmpClient(qmp_socket)

        # 2. Launch the third desktop icon with real input. Music Player is the
        #    first app, so its canonical client origin is (155, 117), 710x448.
        double_click(qmp, 47, 154)
        capture.wait_all(
            (
                "compositor: app 1 connected",
                "compositor: app 1 first scene presented",
            ),
            min(15.0, deadline - time.monotonic()),
        )

        # 3. Every sorted Up Next row is opened through production lite:fs ->
        #    File -> blob: -> <audio>. Generic event counts are sampled before
        #    the click, so an earlier successful codec cannot satisfy a later
        #    one. The two scroll transitions keep the requested row visible.
        app_x, app_y = APP_ORIGIN
        for index, (_, guest_name) in enumerate(FIXTURES):
            if index == 6:
                scroll_queue(qmp, app_x, app_y, QUEUE_FIRST_SCROLL_ROWS)
            elif index == 11:
                scroll_queue(qmp, app_x, app_y, 4)
            source_marker = f"LITE_AUDIO source-opened id=1 file={guest_name}"
            source_before = capture.count(source_marker)
            loaded_before = capture.count("LITE_AUDIO event=loadedmetadata")
            playing_before = capture.count("LITE_AUDIO event=playing")
            if index < 6:
                queue_offset = 0
            elif index < 11:
                queue_offset = QUEUE_FIRST_SCROLL_ROWS * QUEUE_ROW_HEIGHT
            else:
                queue_offset = QUEUE_BOTTOM_OFFSET
            click(
                qmp,
                app_x + QUEUE_ROW_POINT[0],
                app_y + QUEUE_ROW_POINT[1] + index * QUEUE_ROW_HEIGHT - queue_offset,
            )
            capture.wait_new(
                source_marker,
                source_before,
                min(8.0, deadline - time.monotonic()),
            )
            capture.wait_new(
                "LITE_AUDIO event=loadedmetadata",
                loaded_before,
                min(8.0, deadline - time.monotonic()),
            )
            capture.wait_new(
                "LITE_AUDIO event=playing",
                playing_before,
                min(8.0, deadline - time.monotonic()),
            )
            current = wav_frame_count(audio_output)
            wait_for_wav_growth(audio_output, current + 5_000)

            if index != 0:
                continue

            # 4. Cycle the production Repeat control from Off through All to One
            #    before the longer UA workload. Only Repeat: One maps to the
            #    standard media `loop` state used by the service barrier.
            loop_x = app_x + REPEAT_BUTTON_POINT[0]
            loop_y = app_y + REPEAT_BUTTON_POINT[1]
            loop_enabled_before = capture.count("LITE_AUDIO loop id=1 enabled=true")
            click(qmp, loop_x, loop_y)
            time.sleep(0.1)
            click(qmp, loop_x, loop_y)
            capture.wait_new(
                "LITE_AUDIO loop id=1 enabled=true", loop_enabled_before, 5.0
            )

            # 5. Exercise the UA controls on the first track. These coordinates
            #    are derived from the production Now Playing transport, seek
            #    range, and status-bar volume controls in the canonical 710x448
            #    client; no test-only node exists.
            ua_play_x = app_x + PLAY_BUTTON_POINT[0]
            ua_play_y = app_y + PLAY_BUTTON_POINT[1]
            ua_seek_x = app_x + SEEK_POINT[0]
            ua_seek_y = app_y + SEEK_POINT[1]
            ua_mute_x = app_x + ELEMENT_MUTE_POINT[0]
            ua_volume_y = app_y + ELEMENT_MUTE_POINT[1]

            pause_before = capture.count("LITE_AUDIO event=pause")
            click(qmp, ua_play_x, ua_play_y)
            capture.wait_new("LITE_AUDIO event=pause", pause_before, 5.0)
            time.sleep(0.2)
            paused_frames = wav_frame_count(audio_output)
            time.sleep(0.35)
            if wav_frame_count(audio_output) != paused_frames:
                raise RuntimeError("UA pause did not stop device output")

            playing_before = capture.count("LITE_AUDIO event=playing")
            click(qmp, ua_play_x, ua_play_y)
            capture.wait_new("LITE_AUDIO event=playing", playing_before, 5.0)

            seeking_before = capture.count("LITE_AUDIO event=seeking")
            seeked_before = capture.count("LITE_AUDIO event=seeked")
            playing_before = capture.count("LITE_AUDIO event=playing")
            click(qmp, ua_seek_x, ua_seek_y)
            capture.wait_new("LITE_AUDIO event=seeking", seeking_before, 5.0)
            capture.wait_new("LITE_AUDIO event=seeked", seeked_before, 5.0)
            capture.wait_new("LITE_AUDIO event=playing", playing_before, 5.0)

            click(
                qmp,
                app_x + ELEMENT_VOLUME_X[50],
                ua_volume_y,
            )
            audible_start = wait_for_wav_growth(
                audio_output, wav_frame_count(audio_output) + 2_048
            )
            audible_end = wait_for_wav_growth(audio_output, audible_start + 12_000)
            element_audible_window = (audible_start, audible_end)

            muted_before = capture.count(
                "LITE_AUDIO gain-installed id=1 gain=0.000000"
            )
            click(qmp, ua_mute_x, ua_volume_y)
            capture.wait_new(
                "LITE_AUDIO gain-installed id=1 gain=0.000000",
                muted_before,
                5.0,
            )
            # GainInstalled is the mixer-owner barrier. Drain the already queued
            # device periods before measuring silence; otherwise QEMU backend
            # buffering can attribute pre-mute samples to the quiet window.
            quiet_start = wait_for_wav_growth(
                audio_output, wav_frame_count(audio_output) + 2_048
            )
            quiet_end = wait_for_wav_growth(audio_output, quiet_start + 12_000)
            element_quiet_window = (quiet_start, quiet_end)
            click(qmp, ua_mute_x, ua_volume_y)
            click(
                qmp,
                app_x + ELEMENT_VOLUME_X[80],
                ua_volume_y,
            )

            # Require a complete EOF loop after the other controls, so an
            # earlier explicit seek cannot satisfy this generation barrier.
            seeking_before = capture.count("LITE_AUDIO event=seeking")
            seeked_before = capture.count("LITE_AUDIO event=seeked")
            playing_before = capture.count("LITE_AUDIO event=playing")
            capture.wait_new("LITE_AUDIO event=seeking", seeking_before, 5.0)
            capture.wait_new("LITE_AUDIO event=seeked", seeked_before, 5.0)
            capture.wait_new("LITE_AUDIO event=playing", playing_before, 5.0)
            loop_disabled_before = capture.count("LITE_AUDIO loop id=1 enabled=false")
            click(qmp, loop_x, loop_y)
            capture.wait_new(
                "LITE_AUDIO loop id=1 enabled=false", loop_disabled_before, 5.0
            )
            # Keep every remaining codec on the explicitly clicked source.
            # Without this loop, `onEnded` may advance the playlist while the
            # host waits for QEMU's buffered WAV growth and falsely bypass the
            # next physical row click.
            loop_enabled_before = capture.count("LITE_AUDIO loop id=1 enabled=true")
            click(qmp, loop_x, loop_y)
            time.sleep(0.1)
            click(qmp, loop_x, loop_y)
            capture.wait_new(
                "LITE_AUDIO loop id=1 enabled=true", loop_enabled_before, 5.0
            )

        # 6. Exercise the desktop-only system controller through the production
        #    taskbar popup. Exact service markers prove the click reached the
        #    authoritative owner; WAV windows prove the mixer applied it.
        toggle_master_popup(qmp)

        click(qmp, *MASTER_MUTE_POINT)
        capture.wait_new("audio-service: master percent=75 muted=true", 0, 5.0)
        master_quiet_start = wait_for_wav_growth(
            audio_output, wav_frame_count(audio_output) + 2_048
        )
        master_quiet_end = wait_for_wav_growth(
            audio_output, master_quiet_start + 12_000
        )
        master_quiet_window = (master_quiet_start, master_quiet_end)

        click(qmp, *MASTER_MUTE_POINT)
        capture.wait_new("audio-service: master percent=75 muted=false", 0, 5.0)
        click(qmp, MASTER_VOLUME_X[30], MASTER_SCALE_Y)
        capture.wait_new("audio-service: master percent=30 muted=false", 0, 5.0)
        master_low_start = wait_for_wav_growth(
            audio_output, wav_frame_count(audio_output) + 2_048
        )
        master_low_end = wait_for_wav_growth(audio_output, master_low_start + 12_000)
        master_low_window = (master_low_start, master_low_end)

        click(qmp, MASTER_VOLUME_X[70], MASTER_SCALE_Y)
        capture.wait_new("audio-service: master percent=70 muted=false", 0, 5.0)
        master_reference_start = wait_for_wav_growth(
            audio_output, wav_frame_count(audio_output) + 2_048
        )
        master_reference_end = wait_for_wav_growth(
            audio_output, master_reference_start + 12_000
        )
        master_reference_window = (master_reference_start, master_reference_end)
        toggle_master_popup(qmp)

        # 7. Put the first production process on the deterministic high-level
        #    PCM source used by the seven processes below. The 13-format matrix
        #    has already been proven in order; this extra public file is only the
        #    limiter workload and its positive DC level is phase-independent.
        limiter_workload_start = wav_frame_count(audio_output)
        source_before = capture.count(
            f"LITE_AUDIO source-opened id=1 file={LIMITER_FIXTURE[1]}"
        )
        playing_before = capture.count("LITE_AUDIO event=playing")
        click(
            qmp,
            app_x + QUEUE_ROW_POINT[0],
            app_y + QUEUE_ROW_POINT[1] + len(FIXTURES) * QUEUE_ROW_HEIGHT
            - QUEUE_BOTTOM_OFFSET,
        )
        capture.wait_new(
            f"LITE_AUDIO source-opened id=1 file={LIMITER_FIXTURE[1]}",
            source_before,
            min(8.0, deadline - time.monotonic()),
        )
        capture.wait_new(
            "LITE_AUDIO event=playing",
            playing_before,
            min(8.0, deadline - time.monotonic()),
        )

        # Keep all eight stress sources looping, then launch seven more production
        # Music Player processes through the same desktop icon. The first process
        # remains looped from the deterministic codec matrix; each later process
        # enables the same public control after opening its source. This creates
        # eight real service streams without adding a hidden multi-stream app.
        # The 100% master step deliberately crosses the limiter threshold; the
        # private image is restored to 70% after the metrics window.
        toggle_master_popup(qmp)
        click(qmp, MASTER_VOLUME_X[100], MASTER_SCALE_Y)
        capture.wait_new("audio-service: master percent=100 muted=false", 0, 5.0)
        toggle_master_popup(qmp)
        for app_index in range(1, 8):
            presented_marker = (
                f"compositor: app {app_index + 1} first scene presented"
            )
            presented_before = capture.count(presented_marker)
            source_before = capture.count(
                f"LITE_AUDIO source-opened id=1 file={LIMITER_FIXTURE[1]}"
            )
            playing_before = capture.count("LITE_AUDIO event=playing")
            double_click(qmp, 47, 154)
            capture.wait_new(
                presented_marker,
                presented_before,
                min(10.0, deadline - time.monotonic()),
            )
            child_x = APP_ORIGIN[0] + APP_CASCADE[0] * app_index
            child_y = APP_ORIGIN[1] + APP_CASCADE[1] * app_index
            scroll_queue(qmp, child_x, child_y, 9)
            click(
                qmp,
                child_x + QUEUE_ROW_POINT[0],
                child_y + QUEUE_ROW_POINT[1] + len(FIXTURES) * QUEUE_ROW_HEIGHT
                - QUEUE_BOTTOM_OFFSET,
            )
            capture.wait_new(
                f"LITE_AUDIO source-opened id=1 file={LIMITER_FIXTURE[1]}",
                source_before,
                min(8.0, deadline - time.monotonic()),
            )
            capture.wait_new(
                "LITE_AUDIO event=playing",
                playing_before,
                min(8.0, deadline - time.monotonic()),
            )
            loop_enabled_before = capture.count("LITE_AUDIO loop id=1 enabled=true")
            click(
                qmp,
                child_x + REPEAT_BUTTON_POINT[0],
                child_y + REPEAT_BUTTON_POINT[1],
            )
            time.sleep(0.1)
            click(
                qmp,
                child_x + REPEAT_BUTTON_POINT[0],
                child_y + REPEAT_BUTTON_POINT[1],
            )
            capture.wait_new(
                "LITE_AUDIO loop id=1 enabled=true", loop_enabled_before, 5.0
            )

        live_streams = live_stream_ids(capture.text())
        if len(live_streams) != 8:
            raise RuntimeError(
                f"production UI established {len(live_streams)} live streams, expected 8"
            )
        progress_barrier = len(capture.text())
        progress_deadline = min(deadline, time.monotonic() + 5.0)
        while time.monotonic() < progress_deadline:
            concurrent_text = capture.text()[progress_barrier:]
            progressed = {
                stream_id
                for stream_id in live_streams
                if re.search(
                    rf"audio-service: stream progress id={stream_id} "
                    r"generation=\d+ consumed_frames=\d+",
                    concurrent_text,
                )
            }
            if progressed == live_streams:
                break
            time.sleep(0.05)
        else:
            raise RuntimeError(
                "eight production streams did not all progress in one concurrent window"
            )

        # One 188-period window after the concurrency barrier is mandatory;
        # absence cannot pass as a zero or reuse a single-stream warmup metric.
        metrics_before = len(METRICS_RE.findall(capture.text()))
        metrics_deadline = min(deadline, time.monotonic() + 3.0)
        while (
            len(METRICS_RE.findall(capture.text())) <= metrics_before
            and time.monotonic() < metrics_deadline
        ):
            time.sleep(0.05)
        if len(METRICS_RE.findall(capture.text())) <= metrics_before:
            raise RuntimeError("audio service emitted no eight-stream metrics window")
        toggle_master_popup(qmp)
        restored_before = capture.count("audio-service: master percent=70 muted=false")
        click(qmp, MASTER_VOLUME_X[70], MASTER_SCALE_Y)
        capture.wait_new(
            "audio-service: master percent=70 muted=false", restored_before, 5.0
        )
        toggle_master_popup(qmp)
        final_text = capture.text()
        qmp.stop_and_unrealize("audio-device")
        assert_qemu_wav_finalized(audio_output)
        qmp.quit()
        try:
            returncode = process.wait(timeout=3)
        except subprocess.TimeoutExpired as error:
            raise RuntimeError("QEMU did not exit after graceful QMP quit") from error
        if returncode != 0:
            raise RuntimeError(
                f"QEMU exited with status {returncode} after graceful QMP quit"
            )
    finally:
        failed = sys.exc_info()[0] is not None
        if qmp is not None:
            qmp.close()
        capture.close()
        terminate(process)
        final_text = capture.text()
        if failed:
            retain_failure_artifacts()
            private.cleanup()

    try:
        for marker in FATAL_MARKERS:
            if marker in final_text:
                raise RuntimeError(
                    f"audio guest reached fatal marker {marker!r}\n"
                    f"--- output tail ---\n{serial_tail(final_text)}"
                )
        worker_allocations = [int(value) for value in WORKER_ALLOCATIONS_RE.findall(final_text)]
        if len(worker_allocations) < len(FIXTURES):
            raise RuntimeError(
                f"only {len(worker_allocations)} codec allocation markers were emitted"
            )
        if any(worker_allocations):
            raise RuntimeError(
                f"decoder steady-state allocations are non-zero: {worker_allocations!r}"
            )
        first_fixture = FIXTURES[0][1]
        expected_sources = (
            [first_fixture]
            + [guest for _, guest in FIXTURES]
            + [LIMITER_FIXTURE[1]]
            + [source for _ in range(7) for source in (first_fixture, LIMITER_FIXTURE[1])]
        )
        actual_sources = SOURCE_OPENED_RE.findall(final_text)
        if actual_sources != expected_sources:
            raise RuntimeError(
                "production source-opened order differs from the requested codec workload: "
                f"{actual_sources!r}"
            )
        metrics = parse_metrics(final_text)
        if metrics["limiter_activations"] <= 0 or metrics["limiter_max_reduction"] <= 0:
            raise RuntimeError(
                "eight-stream metrics did not exercise the production limiter: "
                f"activations={metrics['limiter_activations']} "
                f"max_reduction={metrics['limiter_max_reduction']}"
            )
        signal, tone_440, tone_660 = validate_signal(
            audio_output,
            limiter_workload_start,
            element_quiet_window,
            element_audible_window,
            master_quiet_window,
            master_low_window,
            master_reference_window,
        )
        return AudioGateResult(
            files_played=len(FIXTURES),
            concurrent_streams=len(live_streams),
            duration_seconds=signal.duration_seconds,
            rms=signal.rms(),
            tone_440=tone_440,
            tone_660=tone_660,
            periods=metrics["periods"],
            xrun=metrics["xrun"],
            steady_allocations=metrics["steady_allocations"],
            idle_periodic_wakes=metrics["idle_periodic_wakes"],
            mix_p99_us=metrics["mix_p99_us"],
            limiter_activations=metrics["limiter_activations"],
            limiter_max_reduction=metrics["limiter_max_reduction"],
        )
    except Exception:
        retain_failure_artifacts()
        raise
    finally:
        private.cleanup()


def gate_inputs(image: Path) -> tuple[Path, ...]:
    """返回会改变 audio runtime verdict 的完整 cache 输入集。"""
    target = target_from_environment()
    fixtures = tuple(
        FIXTURE_DIRECTORY / source for source, _ in (*FIXTURES, LIMITER_FIXTURE)
    )
    return (
        image,
        ROOT / target.kernel_boot_artifact(),
        ROOT / "scripts" / "qemu_gate.py",
        ROOT / "scripts" / "audio_analysis.py",
        ROOT / "scripts" / "ext2_image.py",
        Path(__file__).resolve(),
        *fixtures,
    )


def report(result: AudioGateResult) -> None:
    """打印不持久化本机测量值的当前 invocation 摘要。"""
    print("AArch64/HVF production Music Player audio:")
    print(f"- codec files: {result.files_played}/13 [ok]")
    print(f"- concurrent production streams: {result.concurrent_streams}/8 [ok]")
    print(
        f"- WAV: 48000Hz stereo S16 duration={result.duration_seconds:.3f}s "
        f"rms={result.rms:.6f} [ok]"
    )
    print(f"- tones: 440Hz={result.tone_440:.6f} 660Hz={result.tone_660:.6f} [ok]")
    print(
        f"- metrics: periods={result.periods} xrun={result.xrun} "
        f"steady_allocations={result.steady_allocations} "
        f"idle_periodic_wakes={result.idle_periodic_wakes} "
        f"mix_p99_us={result.mix_p99_us} "
        f"limiter_activations={result.limiter_activations} "
        f"limiter_max_reduction={result.limiter_max_reduction:.6f} [ok]"
    )


def main() -> int:
    target = target_from_environment()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--image",
        type=Path,
        default=ROOT / "target" / "rootfs" / f"{target.arch}.img",
        help="read-only baseline rootfs; fixtures are injected only into a private copy",
    )
    args = parser.parse_args()
    if target.arch != "aarch64":
        print(
            f"audio runtime gate skipped: {target.arch} retains compile/static/boot coverage only"
        )
        return 0
    acceleration = acceleration_from_environment()
    if acceleration != "hvf":
        print(
            f"audio runtime gate skipped: AArch64 ACCEL={acceleration} is diagnostic; "
            "the blocking audio owner is AArch64/HVF"
        )
        return 0
    image = args.image.resolve()
    try:
        if not image.is_file():
            raise RuntimeError(f"rootfs image is missing: {image}")
        stamp = ROOT / "target" / "verify-gates" / "audio-aarch64-hvf.json"
        payload = runtime_gate_payload("audio", RECIPE_VERSION, gate_inputs(image))
        if runtime_gate_hit(stamp, payload, (image,)):
            print("audio runtime verification cache hit")
            return 0
        result = run_audio_gate(image)
        report(result)
        publish_runtime_gate(stamp, payload)
    except RuntimeError as error:
        print(f"audio runtime verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
