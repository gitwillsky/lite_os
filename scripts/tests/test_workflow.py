from __future__ import annotations

import sys
import tomllib
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
ROOT = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))

import workflow  # noqa: E402


class WorkflowTests(unittest.TestCase):
    def test_first_party_workspaces_share_and_inherit_the_strict_clippy_policy(self) -> None:
        root = tomllib.loads((ROOT / "Cargo.toml").read_text())
        policy = root["workspace"]["lints"]["clippy"]
        self.assertEqual(policy["all"], {"level": "deny", "priority": -1})
        required = ("items_after_test_module", "as_ptr_cast_mut", "dbg_macro", "debug_assert_with_mut_call",
                    "fallible_impl_from", "lossy_float_literal", "or_fun_call", "rc_mutex", "todo",
                    "unchecked_time_subtraction", "unimplemented")
        for lint in required:
            self.assertEqual(policy[lint], "deny")
        for base in (ROOT, ROOT / "user"):
            workspace = tomllib.loads((base / "Cargo.toml").read_text())["workspace"]
            self.assertEqual(workspace["lints"]["clippy"], policy)
            for member in workspace["members"]:
                manifest = tomllib.loads((base / member / "Cargo.toml").read_text())
                self.assertTrue(manifest["lints"]["workspace"], member)
        bootloader = tomllib.loads((ROOT / "bootloader/Cargo.toml").read_text())
        self.assertEqual(bootloader["lints"]["clippy"], policy)

    def test_host_gate_covers_user_and_root_test_targets_and_all_formatters(self) -> None:
        with (patch.object(workflow, "run") as run,
              patch.object(workflow, "verify_unit"),
              patch.object(workflow, "verify_architecture_benchmark")):
            workflow.verify_host({"ARCH": "aarch64", "ACCEL": "hvf"})
        commands = [call.args[0] for call in run.call_args_list]
        for manifest in ("user/Cargo.toml", "bootloader/Cargo.toml"):
            self.assertTrue(any(command[:2] == ["cargo", "fmt"] and manifest in command
                                and "--check" in command for command in commands), manifest)
        checks = [command for command in commands if command[:2] == ["cargo", "clippy"]]
        user = next(command for command in checks if "user/Cargo.toml" in command)
        self.assertIn("--workspace", user)
        self.assertIn("--all-targets", user)
        self.assertTrue(any("--manifest-path" not in command and "--all-targets" in command
                            for command in checks))
        for command in checks:
            self.assertEqual(command[-3:], ["--", "-D", "warnings"])

    def test_cache_prune_preview_and_apply_use_only_targeted_script(self) -> None:
        for scope, flags in (("prune-cache-preview", []), ("prune-cache", ["--apply"])):
            with self.subTest(scope=scope), patch.object(workflow, "run") as run:
                workflow.dispatch(scope, {"ARCH": "aarch64", "ACCEL": "hvf"})
                run.assert_called_once()
                command = run.call_args.args[0]
                self.assertEqual(Path(command[1]).name, "prune_build_cache.py")
                self.assertEqual(command[2:], flags)

    def test_release_check_builds_matching_boot_artifact_before_static_gates(self) -> None:
        for arch in ("aarch64", "riscv64"):
            with self.subTest(arch=arch), patch.object(workflow, "run") as run:
                workflow.verify_architecture_release(
                    {"ARCH": arch, "ACCEL": "tcg", "PROFILE": "dev"}
                )
                commands = [call.args[0] for call in run.call_args_list]
                self.assertEqual(commands[0][:2], ["cargo", "build"])
                self.assertIn("--release", commands[0])
                self.assertEqual(Path(commands[1][1]).name, "verify_artifacts.py")
                self.assertEqual(commands[1][2:], ["--build-boot-artifact", "--profile", "release"])
                self.assertEqual(Path(commands[2][1]).name, "verify_architecture_release.py")
                self.assertEqual(Path(commands[3][1]).name, "check_trap_cost.py")
                for call in run.call_args_list:
                    self.assertEqual(call.kwargs["environment"]["PROFILE"], "release")

    def test_unit_scope_runs_the_gate_script_tests(self) -> None:
        fixture = Path("fixture")
        with (
            patch.object(workflow, "run", return_value="assets") as run,
            patch.object(workflow, "kernel_unit_ext4_fixture", return_value=fixture),
            patch.object(workflow, "kernel_unit_dtb_fixtures", return_value={}),
            patch.object(workflow, "find_e2fsck", return_value=fixture),
            patch.object(workflow, "find_debugfs", return_value=fixture),
            patch.object(workflow, "find_tune2fs", return_value=fixture),
        ):
            workflow.verify_unit({"ARCH": "aarch64", "ACCEL": "hvf"})

        command = run.call_args_list[-1].args[0]
        self.assertEqual(command[1:], ["-m", "unittest", "discover", "-s", "scripts/tests", "-p", "test_*.py"])
        self.assertEqual(run.call_args_list[-1].kwargs["environment"]["PYTHONPATH"], str(SCRIPTS))

    def test_scheduler_latency_diagnosis_is_not_part_of_any_gate(self) -> None:
        with patch.object(workflow, "run") as run:
            workflow.verify_runtime_gates({"ARCH": "aarch64", "ACCEL": "hvf"})
        scripts = [Path(call.args[0][1]).name for call in run.call_args_list]
        self.assertNotIn("diagnose_scheduler_latency.py", scripts)

        with patch.object(workflow, "run") as run:
            workflow.diagnose_scheduler_latency({"ARCH": "aarch64", "ACCEL": "hvf"})
        self.assertEqual(Path(run.call_args.args[0][1]).name, "diagnose_scheduler_latency.py")

    def test_runtime_gates_are_one_serial_ordered_scope(self) -> None:
        with patch.object(workflow, "run") as run:
            workflow.verify_runtime_gates({"ARCH": "aarch64", "ACCEL": "hvf"})

        scripts = [Path(call.args[0][1]).name for call in run.call_args_list]
        self.assertEqual(
            scripts,
            [
                "verify_boot.py",
                "verify_shutdown.py",
                "verify_musl.py",
                "verify_rust_std.py",
                "verify_busybox.py",
                "verify_apk_apps.py",
                "verify_audio.py",
                "verify_gpu.py",
                "verify_display.py",
                "verify_wayland.py",
                "verify_desktop.py",
                "verify_scheduler_fairness.py",
            ],
        )

    @patch("workflow.subprocess.Popen")
    @patch("workflow.shutil.which", return_value="/opt/qemu-system-aarch64")
    def test_gdb_qemu_syncs_current_init_before_guest_execution(self, _: object, popen: object) -> None:
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
        prepare_rootfs.assert_not_called()
        sync_userland.assert_called_once()
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
