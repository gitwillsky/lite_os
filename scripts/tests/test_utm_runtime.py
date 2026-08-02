from __future__ import annotations

import plistlib
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import utm_runtime  # noqa: E402


class UtmRuntimeTests(unittest.TestCase):
    def test_configuration_is_one_metal_virtio_gpu_path(self) -> None:
        config = utm_runtime._configuration(memory_mib=2048, cpu_count=6)

        self.assertEqual(config["Backend"], "QEMU")
        self.assertEqual(config["ConfigurationVersion"], 4)
        self.assertEqual(config["System"]["Architecture"], "aarch64")
        self.assertEqual(config["System"]["CPU"], "host")
        self.assertTrue(config["QEMU"]["Hypervisor"])
        self.assertIn("highmem-ecam=off", config["QEMU"]["MachinePropertyOverride"])
        self.assertEqual(
            config["Display"],
            [
                {
                    "Hardware": "virtio-gpu-gl-device",
                    "DynamicResolution": True,
                    "UpscalingFilter": "Linear",
                    "DownscalingFilter": "Linear",
                    "NativeResolution": True,
                }
            ],
        )
        arguments = config["QEMU"]["AdditionalArguments"]
        self.assertNotIn("-display", arguments)
        self.assertIn("format=raw", " ".join(arguments))
        self.assertNotIn("qemu-vdagent", " ".join(arguments))
        self.assertIn("virtio-keyboard-device", arguments)
        self.assertIn("virtio-tablet-device", arguments)
        self.assertEqual(config["Drive"], [])
        self.assertEqual(config["Serial"], [{"Mode": "Ptty", "Target": "Auto"}])
        self.assertTrue(config["Sharing"]["ClipboardSharing"])

    def test_prepare_publishes_same_artifact_inodes_and_current_plist(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "rootfs"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            package = root / "LiteOS.utm"
            artifacts = root / "group" / "LiteOS"
            with (
                patch.object(utm_runtime, "VM_PACKAGE", package),
                patch.object(utm_runtime, "UTM_ARTIFACTS", artifacts),
                patch.object(utm_runtime, "_installed_version", return_value="4.7.5"),
            ):
                result = utm_runtime.prepare(
                    kernel=kernel,
                    rootfs=rootfs,
                    memory="2G",
                    cpu_count=6,
                )

            self.assertEqual(result, package)
            self.assertEqual(kernel.stat().st_ino, (artifacts / "kernel").stat().st_ino)
            self.assertEqual(rootfs.stat().st_ino, (artifacts / "rootfs.img").stat().st_ino)
            with (package / "config.plist").open("rb") as stream:
                config = plistlib.load(stream)
            self.assertEqual(config["System"]["MemorySize"], 2048)
            self.assertEqual(config["System"]["CPUCount"], 6)
            arguments = " ".join(config["QEMU"]["AdditionalArguments"])
            self.assertIn(str(artifacts / "rootfs.img"), arguments)
            self.assertIn(str(artifacts / "kernel"), arguments)

    def test_memory_requires_an_explicit_binary_unit(self) -> None:
        self.assertEqual(utm_runtime._memory_mib("6G"), 6144)
        self.assertEqual(utm_runtime._memory_mib("2048M"), 2048)
        with self.assertRaisesRegex(ValueError, "M or G"):
            utm_runtime._memory_mib("2048")

    def test_visible_start_uses_the_public_utm_scripting_api(self) -> None:
        with patch.object(utm_runtime.subprocess, "run") as run:
            utm_runtime._start_visible()

        arguments = run.call_args.args[0]
        self.assertEqual(arguments[:2], ["osascript", "-e"])
        self.assertIn('application id "com.utmapp.UTM"', arguments[2])
        self.assertIn(utm_runtime.VM_UUID, arguments[2])
        self.assertIn("start vm", arguments[2])
        self.assertNotIn("utmctl start", arguments[2])

    def test_registration_uses_public_import_instead_of_document_open(self) -> None:
        with (
            patch.object(utm_runtime, "_registered", side_effect=[False, True]),
            patch.object(utm_runtime.subprocess, "run") as run,
            patch.object(utm_runtime.time, "sleep"),
        ):
            utm_runtime._ensure_registered()

        commands = [call.args[0] for call in run.call_args_list]
        script = next(command[2] for command in commands if command[:2] == ["osascript", "-e"])
        self.assertIn("import new virtual machine", script)
        self.assertIn(str(utm_runtime.VM_PACKAGE), script)
        self.assertFalse(any(command[:2] == ["open", "-g"] for command in commands))

    def test_status_is_read_from_one_registry_snapshot(self) -> None:
        listing = (
            "UUID Status Name\n"
            f"{utm_runtime.VM_UUID} started LiteOS\n"
        )
        with patch.object(utm_runtime, "_ctl", return_value=listing) as ctl:
            self.assertEqual(utm_runtime._status(), "started")
        ctl.assert_called_once_with("list")

    def test_interrupt_stops_only_the_managed_vm_and_exits_cleanly(self) -> None:
        with (
            patch.object(utm_runtime, "_installed_version"),
            patch.object(utm_runtime, "_remove_registered"),
            patch.object(utm_runtime, "prepare"),
            patch.object(utm_runtime, "_ensure_registered"),
            patch.object(utm_runtime, "_start_visible"),
            patch.object(
                utm_runtime,
                "_status",
                side_effect=["stopped", "started", KeyboardInterrupt],
            ),
            patch.object(utm_runtime, "_registered", return_value=True),
            patch.object(utm_runtime, "_ctl", return_value="") as ctl,
        ):
            utm_runtime.run_gui(
                kernel=Path("kernel"),
                rootfs=Path("rootfs"),
                memory="2G",
                cpu_count=6,
            )

        ctl.assert_called_once_with("stop", utm_runtime.VM_UUID)


if __name__ == "__main__":
    unittest.main()
