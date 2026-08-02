from __future__ import annotations

import sys
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
ROOT = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))

import workflow  # noqa: E402


class WorkflowTests(unittest.TestCase):
    def test_runtime_gates_are_one_serial_ordered_scope(self) -> None:
        with patch.object(workflow, "run") as run:
            workflow.verify_runtime_gates({"ARCH": "aarch64", "ACCEL": "hvf"})

        scripts = [Path(call.args[0][1]).name for call in run.call_args_list]
        self.assertEqual(
            scripts,
            [
                "verify_boot.py",
                "verify_musl.py",
                "verify_rust_std.py",
                "verify_busybox.py",
                "verify_apk_apps.py",
                "verify_audio.py",
                "verify_frame_timing.py",
            ],
        )

    def test_audio_scope_skips_without_launching_a_qemu_gate(self) -> None:
        with patch.object(workflow, "run") as run:
            workflow.verify_runtime_audio({"ARCH": "riscv64", "ACCEL": "tcg"})
        run.assert_not_called()

    @patch("workflow.subprocess.Popen")
    @patch("workflow.shutil.which", return_value="/opt/qemu-system-aarch64")
    def test_gdb_qemu_keeps_rootfs_only_preparation(self, _: object, popen: object) -> None:
        process = popen.return_value
        process.returncode = 0
        with patch.object(workflow, "build_kernel") as build_kernel, patch.object(
            workflow, "build_bootloader"
        ) as build_bootloader, patch.object(workflow, "prepare_rootfs") as prepare_rootfs, patch.object(
            workflow, "sync_userland"
        ) as sync_userland:
            workflow.run_qemu("gdb", {"ARCH": "aarch64", "ACCEL": "hvf", "PROFILE": "release"})

        build_kernel.assert_called_once()
        build_bootloader.assert_called_once()
        prepare_rootfs.assert_called_once()
        sync_userland.assert_not_called()
        process.wait.assert_called_once()

    @patch("workflow.shutil.which", return_value="/opt/qemu-system-aarch64")
    def test_headless_qemu_has_no_gui_backend_override(self, _: object) -> None:
        command = workflow._qemu_command(
            Path("fs-aarch64.img"),
            mode="run",
            memory="2G",
            environment={
                "ARCH": "aarch64",
                "ACCEL": "hvf",
                "PROFILE": "release",
                "QEMU_SMP": "4",
            },
        )
        self.assertEqual(command[0], "/opt/qemu-system-aarch64")
        self.assertEqual(command[command.index("-m") + 1], "2G")
        self.assertEqual(command[command.index("-smp") + 1], "4")
        self.assertIn("-nographic", command)
        self.assertNotIn("-display", command)
        self.assertNotIn("cocoa", " ".join(command))
        self.assertIn("com.redhat.spice.0", " ".join(command))

    def test_make_is_a_thin_dispatcher_without_recursive_make(self) -> None:
        makefile = (ROOT / "Makefile").read_text()
        self.assertNotIn("$(MAKE)", makefile)
        self.assertIn("WORKFLOW := $(PYTHON) scripts/workflow.py", makefile)
        self.assertIn("verify verify-fast verify-runtime", makefile)


if __name__ == "__main__":
    unittest.main()
