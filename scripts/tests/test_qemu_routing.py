from __future__ import annotations

import os
import io
import hashlib
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import build_cache  # noqa: E402
import qemu_gate  # noqa: E402


def argument_after(command: list[str], option: str) -> str:
    return command[command.index(option) + 1]


class QemuRoutingTests(unittest.TestCase):
    def setUp(self) -> None:
        validator = patch("qemu_gate.validate_kernel_boot_artifact")
        self.validate_artifact = validator.start()
        self.addCleanup(validator.stop)

    @patch("qemu_gate.shutil.which", return_value="/opt/qemu")
    def test_second_disk_keeps_root_first_in_each_machine_probe_order(self, _: Mock) -> None:
        for arch, expected in (
            ("aarch64", ["virtio-blk-device,drive=x1", "virtio-blk-device,drive=x0"]),
            ("riscv64", ["virtio-blk-device,drive=x0", "virtio-blk-device,drive=x1"]),
        ):
            with self.subTest(arch=arch), patch.dict(
                os.environ, {"ARCH": arch, "ACCEL": "tcg"}, clear=True
            ):
                command = qemu_gate._qemu_command(
                    Path("root.img"), 1, scratch_disk=Path("swap.img")
                )
                self.assertEqual(
                    [argument for argument in command if argument.startswith("virtio-blk-device")],
                    expected,
                )
                self.assertIn("file=root.img,if=none,format=raw,id=x0", command)
                self.assertIn("file=swap.img,if=none,format=raw,id=x1", command)

    @patch("qemu_gate.shutil.which", return_value="/opt/qemu")
    def test_audio_capture_attaches_one_physical_playback_device(self, _: Mock) -> None:
        with patch.dict(os.environ, {"ARCH": "aarch64", "ACCEL": "hvf"}, clear=True):
            baseline = qemu_gate._qemu_command(Path("root.img"), 4)
            capture = qemu_gate._qemu_command(Path("root.img"), 4, audio_wav=Path("output.wav"))
        self.assertNotIn("-audiodev", baseline)
        self.assertIn("none,id=audio0,out.frequency=48000,out.channels=2,out.format=s16", capture)
        self.assertEqual(capture.count("virtio-sound-device,audiodev=audio0,streams=1"), 1)

    def test_qmp_request_skips_events_and_rejects_command_errors(self) -> None:
        protocol = Mock()
        protocol.readline.side_effect = [b'{"event":"STOP"}\n', b'{"id":"quit","return":{}}\n']
        self.assertEqual(qemu_gate._qmp_execute(protocol, "quit"), {})
        protocol.readline.side_effect = [b'{"id":"quit","error":{"class":"GenericError"}}\n']
        with self.assertRaisesRegex(RuntimeError, "QMP quit"):
            qemu_gate._qmp_execute(protocol, "quit")

    def test_boot_consumes_markers_already_read_by_echo_pacing(self) -> None:
        read_fd, write_fd = os.pipe()
        os.write(write_fd, b"READY")
        os.close(write_fd)
        with os.fdopen(read_fd, "rb") as output_stream:
            process = Mock(stdout=output_stream, stdin=io.BytesIO())
            process.poll.return_value = None
            sent = []

            def inject(_input, _stream, output, _cursor, data):
                sent.append(data.data)
                # This output was consumed by echo pacing, so the next select
                # cannot observe it. Both the next interaction and success need it.
                output.extend(b"AFTER_ECHO" if len(sent) == 1 else b"SUCCESS")

            with (
                patch("qemu_gate.validate_kernel_boot_artifact"),
                patch("qemu_gate._qemu_command", return_value=[]),
                patch("qemu_gate.subprocess.Popen", return_value=process),
                patch("qemu_gate.send_triggered_interaction", side_effect=inject),
                patch("qemu_gate.terminate"),
            ):
                qemu_gate.boot(
                    Path("unused.img"), 1, ("SUCCESS",),
                    interactions=(("READY", qemu_gate.ShellCommand(b"first\n")), ("AFTER_ECHO", qemu_gate.ShellCommand(b"second\n"))),
                    persistent_writes=True,
                )
            self.assertEqual(sent, [b"first\n", b"second\n"])

    def test_foreground_control_keys_do_not_wait_for_a_shell_prompt(self) -> None:
        for key in (b"\x03", b"\x1a", b"q", b"GoOK\x1b:wq\n"):
            with self.subTest(key=key), patch(
                "qemu_gate._wait_for_prompt", side_effect=AssertionError("foreground has no prompt")
            ):
                input_stream = io.BytesIO()
                qemu_gate.send_triggered_interaction(
                    input_stream, io.BytesIO(), bytearray(b"APP_READY"), 9, qemu_gate.TerminalInput(key)
                )
                self.assertEqual(input_stream.getvalue(), key)

    def test_foreground_newline_is_explicit_terminal_input(self) -> None:
        with patch("qemu_gate._wait_for_prompt", side_effect=AssertionError("foreground has no shell prompt")):
            stream = io.BytesIO()
            qemu_gate.send_triggered_interaction(stream, io.BytesIO(), bytearray(), 0,
                qemu_gate.TerminalInput(b"ttyinput\n"))
            self.assertEqual(stream.getvalue(), b"ttyinput\n")

    def test_untyped_interaction_is_rejected_before_qemu_launch(self) -> None:
        with patch("qemu_gate.subprocess.Popen", side_effect=AssertionError("invalid input launched QEMU")):
            with self.assertRaisesRegex(TypeError, "declare ShellCommand or TerminalInput"):
                qemu_gate.boot(Path("unused.img"), 1, (), interactions=(("READY", b"command\n"),))

    def test_missing_actual_prompt_fails_instead_of_blind_submission(self) -> None:
        read_fd, write_fd = os.pipe()
        os.close(write_fd)
        with os.fdopen(read_fd, "rb") as stream:
            with self.assertRaisesRegex(RuntimeError, "shell prompt missing"):
                qemu_gate._wait_for_prompt(stream, bytearray(b"~ # old command"), 4)

    def test_background_readiness_after_actual_prompt_keeps_submission_boundary(self) -> None:
        input_read, input_write = os.pipe()
        output_read, output_write = os.pipe()
        commands = [b"launch background\n", b"consume result\n"]
        received = bytearray()

        def guest_shell() -> None:
            os.write(output_write, b"~ # ")
            for index, command in enumerate(commands):
                for _ in command:
                    byte = os.read(input_read, 1)
                    if not byte:
                        return
                    received.extend(byte)
                    if byte != b"\n":
                        os.write(output_write, byte)
                # 后台事件确实在新 prompt 后出现，不制造第二个 prompt。
                os.write(output_write, b"\n~ # BG_READY\n" if index == 0 else b"\nSUCCESS\n")

        guest = threading.Thread(target=guest_shell, daemon=True)
        guest.start()
        try:
            with os.fdopen(input_write, "wb", closefd=False) as input_stream, os.fdopen(
                output_read, "rb", closefd=False
            ) as output_stream:
                process = Mock(stdout=output_stream, stdin=input_stream)
                process.poll.return_value = None
                with (
                    patch("qemu_gate._qemu_command", return_value=[]),
                    patch("qemu_gate.subprocess.Popen", return_value=process),
                    patch("qemu_gate.terminate"),
                    patch("qemu_gate.PROMPT_WAIT_SECONDS", 0.5),
                ):
                    qemu_gate.boot(Path("unused.img"), 1, ("SUCCESS",),
                        interactions=(("~ # ", qemu_gate.ShellCommand(commands[0])), ("BG_READY", qemu_gate.ShellCommand(commands[1]))),
                        persistent_writes=True)
            guest.join(timeout=1)
            self.assertFalse(guest.is_alive())
            self.assertEqual(received, b"".join(commands))
        finally:
            for descriptor in (input_read, input_write, output_read, output_write):
                os.close(descriptor)

    def test_shell_interaction_waits_for_each_guest_echo(self) -> None:
        input_read, input_write = os.pipe()
        output_read, output_write = os.pipe()
        command = "echo 标准回显\n".encode()
        received = bytearray()

        def echo_guest() -> None:
            while len(received) < len(command):
                byte = os.read(input_read, 1)
                if not byte:
                    return
                received.extend(byte)
                os.write(output_write, byte)

        guest = threading.Thread(target=echo_guest)
        guest.start()
        try:
            with (
                os.fdopen(input_write, "wb", closefd=False) as input_stream,
                os.fdopen(output_read, "rb", closefd=False) as output_stream,
            ):
                output = bytearray()
                qemu_gate.send_shell_interaction(
                    input_stream,
                    output_stream,
                    output,
                    command,
                )
        finally:
            os.close(input_write)
            guest.join()
            os.close(input_read)
            os.close(output_read)
            os.close(output_write)

        self.assertEqual(received, command)
        self.assertEqual(output, command[:-1])

    def test_column_boundary_submits_newline_without_waiting_for_echo(self) -> None:
        input_read, input_write = os.pipe()
        output_read, output_write = os.pipe()
        # 4-column prompt 加 316 glyph 恰好落在 80-column 边界，lineedit 不回显 LF。
        command = b"echo " + b"x" * 311 + b"\n"
        received = bytearray()
        def guest_line_editor() -> None:
            while len(received) < len(command):
                byte = os.read(input_read, 1)
                if not byte:
                    return
                received.extend(byte)
                if byte != b"\n":
                    os.write(output_write, byte)
                else:
                    os.write(output_write, b"~ # ")
        guest = threading.Thread(target=guest_line_editor)
        guest.start()
        try:
            with (
                os.fdopen(input_write, "wb", closefd=False) as input_stream,
                os.fdopen(output_read, "rb", closefd=False) as output_stream,
            ):
                output = bytearray()
                qemu_gate.send_shell_interaction(input_stream, output_stream, output, command)
                guest.join()
                self.assertEqual(received, command)
                self.assertEqual(output, command[:-1])
                self.assertEqual(os.read(output_read, 4), b"~ # ")
        finally:
            os.close(input_write)
            guest.join()
            os.close(input_read)
            os.close(output_read)
            os.close(output_write)

    def test_echo_pacing_rejects_raw_terminal_input(self) -> None:
        self.assertFalse(qemu_gate._is_echo_paced_shell_command(b"first\nsecond\n"))
        with self.assertRaises(ValueError):
            qemu_gate.ShellCommand(b"first\nsecond\n")
        self.assertTrue(qemu_gate._is_echo_paced_shell_command(b"echo ok\n"))
        self.assertTrue(
            qemu_gate._is_echo_paced_shell_command("echo 标准回显\n".encode())
        )
        self.assertFalse(qemu_gate._is_echo_paced_shell_command(b"GoOK\x1b:wq\n"))
        self.assertFalse(qemu_gate._is_echo_paced_shell_command(b"echo \xff\n"))
        self.assertFalse(qemu_gate._is_echo_paced_shell_command(b"q"))

    def test_fatal_line_drain_collects_the_rest_of_the_current_line(self) -> None:
        read_fd, write_fd = os.pipe()
        try:
            with os.fdopen(read_fd, "rb", closefd=False) as stream:
                os.write(write_fd, b"l] access fault\nnext line")
                output = bytearray(b"[CPU-0] [ERROR] [kerne")
                qemu_gate.drain_fatal_line(stream, output)
        finally:
            os.close(read_fd)
            os.close(write_fd)

        self.assertEqual(output, b"[CPU-0] [ERROR] [kernel] access fault\nnext line")

    @patch("qemu_gate.shutil.which", return_value="/opt/qemu-system-aarch64")
    def test_aarch64_hvf_direct_kernel_route(self, _: Mock) -> None:
        with patch.dict(os.environ, {}, clear=True):
            command = qemu_gate._qemu_command(Path("rootfs.img"), 7)

        self.assertEqual(command[0], "/opt/qemu-system-aarch64")
        self.assertEqual(
            argument_after(command, "-machine"),
            "virt,accel=hvf,gic-version=3,its=off,secure=off,"
            "virtualization=off,acpi=off",
        )
        self.assertEqual(argument_after(command, "-cpu"), "host")
        self.assertNotIn("-accel", command)
        self.assertNotIn("-acpi", command)
        self.assertEqual(argument_after(command, "-smp"), "7")
        self.assertNotIn("-m", command)
        self.assertEqual(
            argument_after(command, "-kernel"),
            "target/aarch64-unknown-none-softfloat/release/Image",
        )
        self.assertNotIn("-bios", command)

    @patch("qemu_gate.shutil.which", return_value="/opt/qemu-system-aarch64")
    def test_explicit_noninteractive_memory_is_routed_once(self, _: Mock) -> None:
        with patch.dict(os.environ, {}, clear=True):
            command = qemu_gate._qemu_command(
                Path("rootfs.img"),
                4,
                memory="6G",
            )

        self.assertEqual(command.count("-m"), 1)
        self.assertEqual(argument_after(command, "-m"), "6G")

    @patch("qemu_gate.shutil.which", return_value="/opt/qemu-system-aarch64")
    def test_aarch64_tcg_uses_max_cpu(self, _: Mock) -> None:
        with patch.dict(os.environ, {"ARCH": "aarch64", "ACCEL": "tcg"}, clear=True):
            command = qemu_gate._qemu_command(Path("rootfs.img"), 1)

        self.assertIn("accel=tcg", argument_after(command, "-machine"))
        self.assertEqual(argument_after(command, "-cpu"), "max")

    @patch("qemu_gate.shutil.which", return_value="/opt/qemu-system-riscv64")
    def test_riscv64_tcg_keeps_rustsbi_and_release_kernel(self, _: Mock) -> None:
        environment = {"ARCH": "riscv64", "ACCEL": "tcg"}
        with patch.dict(os.environ, environment, clear=True):
            command = qemu_gate._qemu_command(Path("rootfs.img"), 2)

        self.assertEqual(command[0], "/opt/qemu-system-riscv64")
        self.assertEqual(argument_after(command, "-machine"), "virt,accel=tcg")
        self.assertEqual(argument_after(command, "-cpu"), "rv64")
        self.assertEqual(
            argument_after(command, "-bios"),
            "bootloader/target/riscv64gc-unknown-none-elf/release/bootloader",
        )
        self.assertEqual(
            argument_after(command, "-kernel"),
            "target/riscv64gc-unknown-none-elf/release/kernel",
        )
        self.assertNotIn("-acpi", command)

    def test_riscv64_hvf_fails_before_process_launch(self) -> None:
        with patch.dict(os.environ, {"ARCH": "riscv64"}, clear=True):
            with self.assertRaisesRegex(ValueError, "ACCEL=hvf is not supported"):
                qemu_gate._qemu_command(Path("rootfs.img"), 1)

    def test_runtime_payload_contains_complete_qemu_identity(self) -> None:
        version = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="QEMU emulator version 11.0.1\n",
        )
        with tempfile.TemporaryDirectory() as directory:
            input_path = Path(directory) / "kernel"
            input_path.write_bytes(b"kernel")
            environment = {"ARCH": "aarch64", "ACCEL": "tcg"}
            with (
                patch.dict(os.environ, environment, clear=True),
                patch(
                    "build_cache.shutil.which",
                    return_value="/opt/qemu-system-aarch64",
                ),
                patch("build_cache.subprocess.run", return_value=version),
                patch("build_cache.validate_kernel_boot_artifact", return_value=input_path),
            ):
                payload = build_cache.runtime_gate_payload("boot", 4, (input_path,))

        self.assertEqual(
            payload["qemu"],
            {
                "path": "/opt/qemu-system-aarch64",
                "version": "QEMU emulator version 11.0.1",
                "arch": "aarch64",
                "accel": "tcg",
                "cpu": "max",
                "machine": (
                    "virt,accel=tcg,gic-version=3,its=off,secure=off,"
                    "virtualization=off,acpi=off"
                ),
                "kernel_boot_artifact": (
                    "target/aarch64-unknown-none-softfloat/release/Image"
                ),
                "kernel_boot_sha256": hashlib.sha256(b"kernel").hexdigest(),
            },
        )

    @patch("qemu_gate.shutil.which", return_value=None)
    def test_missing_selected_qemu_reports_binary(self, _: Mock) -> None:
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(RuntimeError, "qemu-system-aarch64 is required"):
                qemu_gate._qemu_command(Path("rootfs.img"), 1)


if __name__ == "__main__":
    unittest.main()
