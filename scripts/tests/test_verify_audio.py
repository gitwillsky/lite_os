#!/usr/bin/env python3
"""Tests for the deterministic parts of the audio runtime gate."""

from __future__ import annotations

import math
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from audio_analysis import WavSignal, read_qemu_wav  # noqa: E402
from verify_audio import (  # noqa: E402
    APP_CASCADE,
    APP_ORIGIN,
    FIXTURES,
    LIMITER_FIXTURE,
    LOOP_STATE_RE,
    MASTER_MUTE_POINT,
    MASTER_SCALE_Y,
    MASTER_SPEAKER_POINT,
    MASTER_VOLUME_X,
    REPEAT_BUTTON_POINT,
    S16_POSITIVE_MAX,
    assert_qemu_wav_finalized,
    channel_tone_amplitude,
    diagnostic_inittab,
    live_stream_ids,
    parse_metrics,
    signal_rms,
    validate_peak_windows,
    wav_frame_count,
)


class AudioRuntimeGateTests(unittest.TestCase):
    def test_audio_diagnostics_are_private_gate_opt_in(self) -> None:
        production = (
            "::respawn:/bin/audio-service\n"
            "::once:/bin/compositor\n"
        )
        self.assertEqual(
            diagnostic_inittab(production),
            "::respawn:/bin/audio-service --diagnostic-log\n"
            "::once:/bin/compositor\n",
        )
        with self.assertRaisesRegex(RuntimeError, "no unique"):
            diagnostic_inittab("::once:/bin/compositor\n")
        with self.assertRaisesRegex(RuntimeError, "no unique"):
            diagnostic_inittab(production + production)

    def test_fixture_guest_names_are_complete_and_sorted(self) -> None:
        self.assertEqual(len(FIXTURES), 13)
        guest_names = [guest for _, guest in FIXTURES]
        self.assertEqual(guest_names, sorted(guest_names))
        self.assertEqual(len(set(guest_names)), len(guest_names))
        self.assertEqual(LIMITER_FIXTURE, ("limiter-dc.wav", "99-limiter-dc.wav"))
        self.assertGreater(LIMITER_FIXTURE[1], guest_names[-1])

    def test_limiter_fixture_is_phase_independent_high_level_stereo(self) -> None:
        signal = read_qemu_wav(SCRIPTS / "fixtures" / "audio" / LIMITER_FIXTURE[0])
        self.assertEqual(signal.sample_rate, 48_000)
        self.assertEqual(signal.duration_seconds, 10.0)
        self.assertAlmostEqual(signal.rms(), 0.25)
        self.assertAlmostEqual(signal.peak(), 0.25)
        self.assertEqual(signal.frames[0], (0.25, 0.25))
        self.assertEqual(signal.frames[-1], (0.25, 0.25))

    def test_production_repeat_hit_point_matches_vqa_and_all_cascades(self) -> None:
        self.assertEqual(
            (
                APP_ORIGIN[0] + REPEAT_BUTTON_POINT[0],
                APP_ORIGIN[1] + REPEAT_BUTTON_POINT[1],
            ),
            (698, 137),
        )
        for index in range(8):
            x = APP_ORIGIN[0] + APP_CASCADE[0] * index + REPEAT_BUTTON_POINT[0]
            y = APP_ORIGIN[1] + APP_CASCADE[1] * index + REPEAT_BUTTON_POINT[1]
            self.assertLess(x, 1504)
            self.assertLess(y, 816)
        self.assertEqual(
            LOOP_STATE_RE.findall(
                "LITE_AUDIO loop id=1 enabled=true\n"
                "LITE_AUDIO loop id=1 enabled=false\n"
            ),
            [("1", "true"), ("1", "false")],
        )

    def test_desktop_master_hit_points_match_production_popup_vqa(self) -> None:
        self.assertEqual(MASTER_SPEAKER_POINT, (1429, 831))
        self.assertEqual(MASTER_MUTE_POINT, (1407, 795))
        self.assertEqual(MASTER_SCALE_Y, 749)
        self.assertEqual(MASTER_VOLUME_X, {30: 1380, 70: 1436, 100: 1478})
        self.assertEqual(
            MASTER_VOLUME_X[70] - MASTER_VOLUME_X[30],
            4 * (MASTER_VOLUME_X[100] - MASTER_VOLUME_X[70]) // 3,
        )

    def test_metrics_rejects_any_historical_regression(self) -> None:
        healthy = (
            "audio-service: metrics periods=188 xrun=0 steady_allocations=0 "
            "idle_periodic_wakes=0 mix_p99_us=250 limiter_activations=0 "
            "limiter_max_reduction=0.000000\n"
        )
        self.assertEqual(parse_metrics(healthy)["periods"], 188)
        with self.assertRaisesRegex(RuntimeError, "violated"):
            parse_metrics(
                healthy
                + "audio-service: metrics periods=376 xrun=1 steady_allocations=0 "
                "idle_periodic_wakes=0 mix_p99_us=250 limiter_activations=0 "
                "limiter_max_reduction=0.000000\n"
            )
        with self.assertRaisesRegex(RuntimeError, "no complete"):
            parse_metrics("")

    def test_live_stream_projection_respects_close_order(self) -> None:
        text = "\n".join(
            (
                "audio-service: stream start id=1 generation=1",
                "audio-service: stream start id=2 generation=1",
                "audio-service: stream close id=1 generation=1 consumed_frames=256",
                "audio-service: stream start id=3 generation=2",
            )
        )
        self.assertEqual(live_stream_ids(text), {2, 3})

    def test_signal_rms_bounds_the_requested_window(self) -> None:
        signal = WavSignal(((0.5, -0.5), (0.0, 0.0)), 48_000)
        self.assertAlmostEqual(signal_rms(signal, 0, 1), 0.5)
        self.assertEqual(signal_rms(signal, 1, 2), 0.0)
        self.assertEqual(signal_rms(signal, 20, 30), 0.0)

    def test_channel_tone_amplitude_preserves_stereo_identity(self) -> None:
        frames = tuple(
            (
                0.5 * math.sin(2.0 * math.pi * 440.0 * index / 48_000),
                0.25 * math.sin(2.0 * math.pi * 660.0 * index / 48_000),
            )
            for index in range(4_800)
        )
        signal = WavSignal(frames, 48_000)
        self.assertAlmostEqual(channel_tone_amplitude(signal, 0, 440.0), 0.5, places=4)
        self.assertAlmostEqual(channel_tone_amplitude(signal, 1, 660.0), 0.25, places=4)
        with self.assertRaisesRegex(ValueError, "channel"):
            channel_tone_amplitude(signal, 2, 440.0)

    def test_selected_reference_window_survives_later_phase_reset(self) -> None:
        tone = tuple(
            (
                0.5 * math.sin(2.0 * math.pi * 440.0 * index / 48_000),
                0.0,
            )
            for index in range(4_800)
        )
        phase_reset = tuple((-left, right) for left, right in tone)
        signal = WavSignal(tone + phase_reset, 48_000)
        reference = WavSignal(signal.frames[: len(tone)], signal.sample_rate)

        self.assertAlmostEqual(channel_tone_amplitude(signal, 0, 440.0), 0.0)
        self.assertAlmostEqual(
            channel_tone_amplitude(reference, 0, 440.0),
            0.5,
            places=4,
        )

    def test_peak_windows_reject_full_scale_before_stress(self) -> None:
        signal = WavSignal(((1.0, 0.0), (0.5, 0.5)), 48_000)

        with self.assertRaisesRegex(RuntimeError, "before limiter stress"):
            validate_peak_windows(signal, 1)

    def test_peak_windows_accept_s16_ceiling_only_during_stress(self) -> None:
        signal = WavSignal(
            ((0.75, -0.75), (S16_POSITIVE_MAX, S16_POSITIVE_MAX)),
            48_000,
        )

        validate_peak_windows(signal, 1)

    def test_peak_windows_reject_negative_s16_wrap_during_stress(self) -> None:
        signal = WavSignal(((0.75, -0.75), (-1.0, -1.0)), 48_000)

        with self.assertRaisesRegex(RuntimeError, "positive S16"):
            validate_peak_windows(signal, 1)

    def test_in_progress_wav_frame_count_never_counts_header(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "audio.wav"
            self.assertEqual(wav_frame_count(path), 0)
            path.write_bytes(b"\0" * 44 + b"\0" * 12)
            self.assertEqual(wav_frame_count(path), 3)

    def test_finalized_wav_header_must_match_the_payload_exactly(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "audio.wav"
            payload = b"\0" * 16
            header = bytearray(44)
            header[:4] = b"RIFF"
            header[4:8] = (36 + len(payload)).to_bytes(4, "little")
            header[8:12] = b"WAVE"
            header[36:40] = b"data"
            header[40:44] = len(payload).to_bytes(4, "little")
            path.write_bytes(header + payload)

            assert_qemu_wav_finalized(path)

            header[40:44] = (len(payload) - 4).to_bytes(4, "little")
            path.write_bytes(header + payload)
            with self.assertRaisesRegex(RuntimeError, "do not match"):
                assert_qemu_wav_finalized(path)


if __name__ == "__main__":
    unittest.main()
