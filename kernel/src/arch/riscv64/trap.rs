use core::arch::asm;

use riscv::{
    ExceptionNumber, InterruptNumber,
    interrupt::{Exception, Interrupt, Trap},
    register::{
        scause, sepc, stval,
        stvec::{self, TrapMode},
    },
};

use super::mmu::AddressSpaceToken;

/// @description User trampoline 跳转目标的 opaque architecture entry。
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct UserTrapEntry(usize);

impl UserTrapEntry {
    /// @description 仅向同 backend 的 UserContext 编码 trampoline target。
    /// @return linked architecture entry address。
    pub(super) fn encoded(self) -> usize {
        self.0
    }
}

/// @description 返回 task context 应保存的 opaque user trap entry。
/// @return 当前 RISC-V backend 的 typed entry token。
pub(crate) fn user_entry() -> UserTrapEntry {
    // SAFETY: entry codec exports this symbol with the trap.S user-entry ABI.
    unsafe extern "C" {
        fn __liteos_user_trap() -> !;
    }
    UserTrapEntry(__liteos_user_trap as *const () as usize)
}

/// @description ISA-neutral trap event delivered to the kernel trap domain。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrapEvent {
    TimerInterrupt,
    ExternalInterrupt,
    SoftwareInterrupt,
    UnsupportedInterrupt,
    IllegalInstruction,
    Breakpoint,
    UserEnvironmentCall,
    InstructionPageFault { address: usize },
    LoadPageFault { address: usize },
    StorePageFault { address: usize },
    LoadAccessFault { address: usize },
    StoreAccessFault { address: usize },
    UnsupportedException { address: usize },
}

/// @description Kernel exception diagnostic with raw CSR encoding hidden inside arch backend。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KernelException {
    pub(crate) event: TrapEvent,
    pub(crate) program_counter: usize,
}

/// @description 解码当前 RISC-V trap CSR 为通用语义事件。
pub(crate) fn event() -> TrapEvent {
    let cause = scause::read().cause();
    let address = stval::read();
    match cause {
        Trap::Interrupt(code) => match Interrupt::from_number(code) {
            Ok(Interrupt::SupervisorTimer) => TrapEvent::TimerInterrupt,
            Ok(Interrupt::SupervisorExternal) => TrapEvent::ExternalInterrupt,
            Ok(Interrupt::SupervisorSoft) => TrapEvent::SoftwareInterrupt,
            Ok(_) | Err(_) => TrapEvent::UnsupportedInterrupt,
        },
        Trap::Exception(code) => match Exception::from_number(code) {
            Ok(Exception::IllegalInstruction) => TrapEvent::IllegalInstruction,
            Ok(Exception::Breakpoint) => TrapEvent::Breakpoint,
            Ok(Exception::UserEnvCall) => TrapEvent::UserEnvironmentCall,
            Ok(Exception::InstructionPageFault) => TrapEvent::InstructionPageFault { address },
            Ok(Exception::LoadPageFault) => TrapEvent::LoadPageFault { address },
            Ok(Exception::StorePageFault) => TrapEvent::StorePageFault { address },
            Ok(Exception::LoadFault) => TrapEvent::LoadAccessFault { address },
            Ok(Exception::StoreFault) => TrapEvent::StoreAccessFault { address },
            Ok(_) | Err(_) => TrapEvent::UnsupportedException { address },
        },
    }
}

/// @description 捕获已解码 kernel trap 的 architecture diagnostic snapshot。
/// @param event generic trap domain 已取得的唯一语义事件。
/// @return 同一事件与当前 program counter 的一致诊断值。
pub(crate) fn kernel_exception(event: TrapEvent) -> KernelException {
    KernelException {
        event,
        program_counter: sepc::read(),
    }
}

/// @description 安装 linked kernel trap entry 到当前 CPU。
pub(crate) fn install_kernel_entry() {
    // SAFETY: trap.S defines this linked entry with the RISC-V supervisor trap ABI.
    unsafe extern "C" {
        fn __kernel_trap();
    }
    let mut value = stvec::Stvec::from_bits(0);
    value.set_address(__kernel_trap as *const () as usize);
    value.set_trap_mode(TrapMode::Direct);
    // SAFETY: linked symbol is aligned trap text and stvec is CPU-local S-mode state.
    unsafe { stvec::write(value) };
}

/// @description 完成 RISC-V trampoline restore 并返回指定用户 address space。
///
/// @param context_address 用户映射中的 UserContext virtual address。
/// @param address_space live Sv39 address-space token。
/// @param trampoline_address 每个 address space 中统一映射的 trampoline virtual address。
/// @return 不返回。
pub(crate) fn return_to_user(
    context_address: usize,
    address_space: AddressSpaceToken,
    trampoline_address: usize,
) -> ! {
    // SAFETY: trap.S defines both symbols in one trampoline section with a stable relative offset.
    unsafe extern "C" {
        fn __restore();
        fn __alltraps();
    }
    let restore =
        __restore as *const () as usize - __alltraps as *const () as usize + trampoline_address;
    let mut value = stvec::Stvec::from_bits(0);
    value.set_address(trampoline_address);
    value.set_trap_mode(TrapMode::Direct);
    // SAFETY: trampoline is aligned executable memory shared by every live user address space.
    unsafe { stvec::write(value) };

    // SAFETY: restore points into the linked trampoline; context and address-space token belong to
    // the current live task. The assembly switches address spaces and never returns to this frame.
    unsafe {
        asm!(
            "fence.i",
            "jr {restore}",
            restore = in(reg) restore,
            in("x10") context_address,
            in("x11") address_space.encoded(),
            options(noreturn)
        )
    }
}
