from __future__ import annotations

import struct
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import check_trap_cost  # noqa: E402
import verify_architecture_release  # noqa: E402
import verify_artifacts  # noqa: E402
from build_target import target_from_environment  # noqa: E402


class TrapCostTests(unittest.TestCase):
    @staticmethod
    def fp_helper(operation: str) -> list[str]:
        lines = ["msr cpacr_el1, x9", "isb"]
        lines.extend(
            f"{operation} q{register}, q{register + 1}, [x0]"
            for register in range(0, 32, 2)
        )
        if operation == "stp":
            lines.extend(("mrs x9, fpcr", "mrs x10, fpsr"))
        else:
            lines.extend(("msr fpcr, x9", "msr fpsr, x10"))
        lines.extend(("msr cpacr_el1, x9", "isb"))
        return lines

    @staticmethod
    def context_switch() -> list[str]:
        lines = ["msr cpacr_el1, x9", "isb"]
        for register in range(0, 32, 2):
            lines.append(f"stp q{register}, q{register + 1}, [x0]")
        lines.extend(("mrs x9, fpcr", "mrs x10, fpsr"))
        lines.extend(("msr fpcr, x9", "msr fpsr, x10"))
        for register in range(0, 32, 2):
            lines.append(f"ldp q{register}, q{register + 1}, [x1]")
        lines.extend(("msr cpacr_el1, x9", "isb"))
        return lines

    @staticmethod
    def user_trap_assembly() -> list[str]:
        return [
            "sub sp, sp, #0x20",
            "sub sp, sp, #0x20",
            "add sp, sp, #0x20",
            "msr tpidr_el0, x9",
            "msr tpidrro_el0, xzr",
            "msr ttbr0_el1, x1",
            "isb",
            "msr vbar_el1, x2",
            "add x11, sp, #0x30",
        ]

    @staticmethod
    def kernel_irq() -> list[str]:
        return [
            "mrs x8, elr_el1",
            "adr x9, __local_irq_wait_wfi",
            "adr x8, __local_irq_wait_wfi_resume",
            "msr elr_el1, x8",
        ]

    @staticmethod
    def bootstrap_wait() -> list[str]:
        return ["msr daifclr, #0x2", "wfi", "msr daifset, #0x2"]

    def test_riscv_limits_preserve_existing_contract(self) -> None:
        measured, limits = check_trap_cost.riscv_measurements(
            ["csrw satp, a0", "csrw satp, a1"], []
        )
        self.assertEqual(measured["satp_writes"], 2)
        self.assertEqual(limits["satp_writes"], 2)
        self.assertEqual(limits["full_sfence_vma"], 0)
        self.assertEqual(limits["fp_stores"], 0)
        self.assertEqual(limits["fp_loads"], 0)
        self.assertEqual(limits["fence_i"], 0)

    def test_aarch64_requires_each_q_register_once_in_each_direction(self) -> None:
        measured, expected = check_trap_cost.aarch64_measurements(
            ["stp x0, x1, [sp]", "ldr x0, [x1]"],
            self.user_trap_assembly(),
            self.kernel_irq(),
            self.bootstrap_wait(),
            [],
            self.context_switch(),
            self.fp_helper("stp"),
            self.fp_helper("ldp"),
            self.fp_helper("stp"),
            self.fp_helper("ldp"),
        )
        self.assertEqual(measured, expected)

    def test_aarch64_rejects_duplicate_q_register_even_at_same_count(self) -> None:
        context_switch = self.context_switch()
        context_switch[-3] = "ldp q30, q30, [x1]"
        measured, expected = check_trap_cost.aarch64_measurements(
            [],
            self.user_trap_assembly(),
            self.kernel_irq(),
            self.bootstrap_wait(),
            [],
            context_switch,
            self.fp_helper("stp"),
            self.fp_helper("ldp"),
            self.fp_helper("stp"),
            self.fp_helper("ldp"),
        )
        self.assertNotEqual(
            measured["switch_restored_q_register_set"],
            expected["switch_restored_q_register_set"],
        )

    def test_aarch64_rejects_system_register_user_trap_scratch(self) -> None:
        user_trap = self.user_trap_assembly()
        user_trap.extend(
            (
                "msr contextidr_el1, x9",
                "mrs x9, tpidrro_el0",
                "msr tpidr_el0, x11",
                "add x11, sp, #0x30",
                "ldr x11, [sp, #0x28]",
                "str x16, [sp, #0]",
            )
        )
        measured, expected = check_trap_cost.aarch64_measurements(
            [],
            user_trap,
            self.kernel_irq(),
            self.bootstrap_wait(),
            [],
            self.context_switch(),
            self.fp_helper("stp"),
            self.fp_helper("ldp"),
            self.fp_helper("stp"),
            self.fp_helper("ldp"),
        )
        for metric in (
            "user_contextidr_writes",
            "user_tpidrro_reads",
            "user_tpidr_el0_writes",
            "user_context_stack_address_adds",
            "user_context_metadata_loads",
            "user_context_metadata_stores",
        ):
            self.assertNotEqual(measured[metric], expected[metric])


class AArch64InstructionContainmentTests(unittest.TestCase):
    def test_only_explicit_helpers_may_touch_fp_state(self) -> None:
        disassembly = "\n".join(
            (
                "00000010 <__switch>:",
                "  10: stp q0, q1, [x0]",
                "  14: mrs x9, FPCR",
                "00000018 <__aarch64_signal_fp_capture>:",
                "  18: stp q0, q1, [x0]",
                "00000020 <kernel::trap::ordinary>:",
                "  20: stp x0, x1, [sp]",
            )
        )
        verify_architecture_release.verify_aarch64_instruction_containment(disassembly)

    def test_fp_register_outside_switch_is_rejected(self) -> None:
        disassembly = "00000020 <kernel::trap::ordinary>:\n  20: str q0, [x1]"
        with self.assertRaisesRegex(RuntimeError, "escapes explicit boundary"):
            verify_architecture_release.verify_aarch64_instruction_containment(
                disassembly
            )

    def test_optional_memory_and_control_extensions_are_rejected(self) -> None:
        for instruction in (
            "paciasp",
            "irg x0, x1",
            "stg x0, [x1]",
            "smstart sm",
            "rdvl x0, #1",
            "rdsvl x0, #1",
            "mrs x0, SVCR",
            "dc gzva, x0",
        ):
            with self.subTest(instruction=instruction):
                disassembly = f"00000020 <__switch>:\n  20: {instruction}"
                with self.assertRaisesRegex(RuntimeError, "forbidden"):
                    verify_architecture_release.verify_aarch64_instruction_containment(
                        disassembly
                    )

    def test_sve_and_sme_registers_are_rejected(self) -> None:
        for instruction in ("add z0.s, z1.s, z2.s", "zero {za}"):
            with self.subTest(instruction=instruction):
                disassembly = f"00000020 <kernel::worker>:\n  20: {instruction}"
                with self.assertRaisesRegex(RuntimeError, "forbidden SVE/SME"):
                    verify_architecture_release.verify_aarch64_instruction_containment(
                        disassembly
                    )


class ArtifactRoutingTests(unittest.TestCase):
    def test_aarch64_uses_release_kernel_without_bootloader(self) -> None:
        target = target_from_environment({"ARCH": "aarch64"})
        specs = verify_artifacts.target_artifacts(target, Path("/cache/busybox"))
        self.assertEqual(len(specs), 2)
        self.assertEqual(
            specs[0].path,
            verify_artifacts.ROOT
            / "target/aarch64-unknown-none-softfloat/release/kernel",
        )
        self.assertIn("AArch64", specs[0].markers)
        self.assertIn("/lib/ld-musl-aarch64.so.1", specs[1].markers)

    def test_riscv_uses_release_kernel_and_bootloader(self) -> None:
        target = target_from_environment({"ARCH": "riscv64"})
        specs = verify_artifacts.target_artifacts(target, Path("/cache/busybox"))
        self.assertEqual(len(specs), 3)
        self.assertIn("bootloader", str(specs[1].path))
        self.assertIn("RISC-V", specs[0].markers)
        self.assertIn("/lib/ld-musl-riscv64.so.1", specs[2].markers)

    def test_writable_executable_load_is_rejected(self) -> None:
        output = "LOAD 0x0 0x0 0x0 0x100 0x100 RWE 0x1000"
        with self.assertRaisesRegex(RuntimeError, "writable executable"):
            verify_artifacts.require(output, "kernel", ())


class AArch64ImageHeaderTests(unittest.TestCase):
    def make_elf(
        self,
        path: Path,
        *,
        text_offset: int = verify_artifacts.AARCH64_TEXT_OFFSET,
        image_size: int = 0x3000,
        physical: int = 0x40080000,
        magic: bytes = verify_artifacts.AARCH64_IMAGE_MAGIC,
    ) -> tuple[int, int, int, bytes]:
        entry = physical
        high_virtual = 0xFFFFFFC000081000
        high_physical = physical + 0x1000
        data = bytearray(0x380)
        identity = bytearray(16)
        identity[:6] = b"\x7fELF\x02\x01"
        struct.pack_into(
            "<16sHHIQQQIHHHHHH",
            data,
            0,
            bytes(identity),
            2,
            verify_artifacts.AARCH64_MACHINE,
            2,
            entry,
            64,
            0,
            0,
            64,
            56,
            2,
            64,
            0,
            0,
        )
        struct.pack_into(
            "<IIQQQQQQ",
            data,
            64,
            verify_artifacts.PT_LOAD,
            5,
            0x200,
            entry,
            physical,
            0x80,
            0x80,
            0x1000,
        )
        struct.pack_into(
            "<IIQQQQQQ",
            data,
            120,
            verify_artifacts.PT_LOAD,
            6,
            0x300,
            high_virtual,
            high_physical,
            0x80,
            image_size - 0x1000,
            0x1000,
        )
        struct.pack_into("<QQ", data, 0x208, text_offset, image_size)
        data[0x238:0x23C] = magic
        data[0x300:0x380] = bytes(range(0x80))
        path.write_bytes(data)
        return entry, high_virtual, high_virtual + image_size - 0x1000, bytes(data)

    def test_header_and_symbols_use_full_physical_image_extent(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            elf = Path(directory) / "kernel"
            entry, start, end, _ = self.make_elf(elf)
            layout_end = 0x40083000
            layout = verify_artifacts.require_aarch64_elf_contract(
                elf,
                "\n".join(
                    (
                        f"{entry:016x} T _start",
                        f"{start:016x} T skernel",
                        f"{end:016x} B ekernel",
                        f"{layout_end:016x} A kernel_image_end_phys",
                    )
                ),
            )
            self.assertEqual(layout.end_physical, layout_end)
            self.assertGreater(layout.image_size, end - start)

    def test_raw_image_matches_each_load_segment_and_zero_fills_gaps(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            elf = Path(directory) / "kernel"
            raw = Path(directory) / "Image"
            _, _, _, elf_bytes = self.make_elf(elf)
            layout = verify_artifacts.aarch64_elf_layout(elf)
            raw.write_bytes(verify_artifacts.expected_aarch64_raw_image(elf, layout))
            verify_artifacts.require_aarch64_raw_image(elf, raw)
            image = raw.read_bytes()
            self.assertEqual(image[:0x80], elf_bytes[0x200:0x280])
            self.assertEqual(image[0x80:0x1000], bytes(0xF80))
            self.assertEqual(image[0x1000:0x1080], elf_bytes[0x300:0x380])

    def test_header_rejects_wrong_offset_magic_and_physical_placement(self) -> None:
        cases = (
            {"text_offset": 0x40000},
            {"magic": b"NOPE"},
            {"physical": 0x40100000},
        )
        for arguments in cases:
            with self.subTest(arguments=arguments), tempfile.TemporaryDirectory() as directory:
                elf = Path(directory) / "kernel"
                self.make_elf(elf, **arguments)
                with self.assertRaises(RuntimeError):
                    verify_artifacts.aarch64_elf_layout(elf)

    def test_failed_generation_preserves_previous_image_and_removes_temporary(self) -> None:
        target = target_from_environment({"ARCH": "aarch64"})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            elf = root / target.kernel_elf()
            image = root / target.kernel_boot_artifact()
            elf.parent.mkdir(parents=True)
            self.make_elf(elf)
            image.write_bytes(b"previous complete Image")

            def fail_after_partial_output(command: list[str]) -> str:
                Path(command[-1]).write_bytes(b"partial")
                raise RuntimeError("objcopy failed")

            with (
                patch.object(verify_artifacts, "ROOT", root),
                patch.object(
                    verify_artifacts,
                    "pinned_rust_objcopy",
                    return_value=Path("/pinned/llvm-objcopy"),
                ),
                patch.object(
                    verify_artifacts,
                    "run_host",
                    side_effect=fail_after_partial_output,
                ),
                self.assertRaisesRegex(RuntimeError, "objcopy failed"),
            ):
                verify_artifacts.build_kernel_boot_artifact(target, "release")

            self.assertEqual(image.read_bytes(), b"previous complete Image")
            self.assertEqual(list(image.parent.glob(".Image.*.tmp")), [])


if __name__ == "__main__":
    unittest.main()
