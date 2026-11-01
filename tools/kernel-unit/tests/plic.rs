//! Execute the production PLIC register owner against an aligned host MMIO window.
extern crate alloc;

mod arch {
    pub(crate) mod cpu {
        pub(crate) struct StartupCpu;
        impl StartupCpu {
            pub(crate) fn new(_: usize, _: usize) -> Self {
                Self
            }
        }
        pub(crate) fn initialize_startup(_: impl Iterator<Item = StartupCpu>) {}
        pub(crate) fn install_boot_cpu(_: usize) {}
        pub(crate) fn current_logical_id() -> usize {
            0
        }
        pub(crate) fn entry_identity() -> usize {
            0
        }
    }
}
#[expect(
    dead_code,
    reason = "host fixture uses actual CPU identities/topology without secondary startup or lifecycle"
)]
#[path = "../../../kernel/src/execution/cpu/mod.rs"]
pub(crate) mod actual_cpu;
#[expect(
    dead_code,
    reason = "register lifecycle fixture does not dispatch live device interrupts"
)]
#[path = "../../../kernel/src/execution/interrupt.rs"]
pub(crate) mod actual_interrupt;
mod execution {
    pub(crate) use crate::{actual_cpu as cpu, actual_interrupt as interrupt};
}
#[path = "../../../kernel/src/platform/qemu_virt/riscv64"]
mod controller {
    #[expect(
        dead_code,
        reason = "hardware claim side effects are validated in the real QEMU runtime"
    )]
    mod plic;
    mod plic_policy;

    #[test]
    fn level_mask_preserves_claim_context_enable_and_unmask_restores_priority() {
        use crate::execution::{
            cpu,
            interrupt::{InterruptError, InterruptHandler},
        };
        use alloc::sync::Arc;
        cpu::initialize(
            [
                cpu::HardwareCpuId::from_raw(0),
                cpu::HardwareCpuId::from_raw(1),
            ],
            cpu::HardwareCpuId::from_raw(0),
        );
        struct Handler;
        impl InterruptHandler for Handler {
            fn handle_interrupt(&self, _: u32) -> Result<(), InterruptError> {
                Ok(())
            }
        }
        let mut registers = vec![0u32; 0x204000 / 4];
        let mut controller = plic::PlicInterruptController::new(
            registers.as_mut_ptr() as usize,
            registers.len() * 4,
            cpu::possible(),
        )
        .unwrap();
        controller.register_handler(33, Arc::new(Handler)).unwrap();
        controller
            .set_affinity(33, cpu::CpuSet::singleton(cpu::id_at(1).unwrap()))
            .unwrap();
        controller.enable_interrupt(33).unwrap();
        assert_eq!(registers[33], 1);
        let cpu0 = (0x2000 + 0x80 + 4) / 4;
        let cpu1 = (0x2000 + 3 * 0x80 + 4) / 4;
        assert_eq!(registers[cpu0], 0);
        assert_eq!(registers[cpu1], 2);
        controller.disable_interrupt(33);
        assert_eq!(
            registers[33], 0,
            "masking must suppress priority before driver acknowledgement"
        );
        assert_eq!(
            registers[cpu1], 2,
            "a disabled claim context may reject completion"
        );
        controller.enable_interrupt(33).unwrap();
        assert_eq!(registers[33], 1);
        assert_eq!(registers[cpu0], 0);
        assert_eq!(registers[cpu1], 2);
        controller.disable_interrupt(33);
        controller
            .set_affinity(33, cpu::CpuSet::singleton(cpu::id_at(0).unwrap()))
            .unwrap();
        assert_eq!(
            registers[33], 0,
            "affinity must not unmask a source awaiting driver ack"
        );
        assert_eq!(registers[cpu0], 2);
        assert_eq!(registers[cpu1], 0);
        controller.enable_interrupt(33).unwrap();
        assert_eq!(registers[33], 1);
    }
}
