use super::{mmu::KernelTrapToken, trap::UserTrapEntry};
use crate::arch::{IllegalInstructionFault, IllegalInstructionProbe, IllegalInstructionRetry};

/// 每任务 kernel stack 顶部由 AArch64 UserContext 与 trap metadata 独占的页数。
pub(crate) const KERNEL_STACK_CONTEXT_RESERVE: usize = super::mmu::PAGE_SIZE;
pub(super) const KERNEL_STACK_CONTEXT_OFFSET: usize = 16;
/// AArch64 keeps the EL0 register image in the Thread's TTBR1 kernel-stack reservation.
pub(crate) const USER_CONTEXT_PLACEMENT: crate::arch::UserContextPlacement =
    crate::arch::UserContextPlacement::KernelStack {
        offset: KERNEL_STACK_CONTEXT_OFFSET,
    };

/// EL0 integer/system state shared by trap entry and user return assembly.
#[repr(C, align(16))]
#[derive(Debug, Clone)]
pub(crate) struct UserContext {
    pub(super) x: [usize; 31],
    pub(super) sp: usize,
    pub(super) pc: usize,
    pub(super) pstate: usize,
    pub(super) kernel_ttbr: u64,
    pub(super) kernel_sp: usize,
    pub(super) trap_handler: usize,
    pub(super) kernel_cpu_id: usize,
    pub(super) thread_pointer: usize,
}

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(offset_of!(UserContext, sp) == 248);
    assert!(offset_of!(UserContext, kernel_ttbr) == 272);
    assert!(size_of::<UserContext>() == 320);
    assert!(KERNEL_STACK_CONTEXT_OFFSET + size_of::<UserContext>() <= KERNEL_STACK_CONTEXT_RESERVE);
};

/// 判断 UserContext 是否由 AArch64 TTBR1 kernel-stack window 保活。
///
/// # Parameters
///
/// - `address`: context virtual address。
///
/// # Returns
///
/// 地址位于 kernel stack window 时为 true。
pub(crate) fn is_kernel_stack_user_context(address: usize) -> bool {
    (super::mmu::KERNEL_STACK_REGION_START..super::mmu::KERNEL_STACK_REGION_TOP).contains(&address)
}

impl UserContext {
    /// Set the EL0 stack pointer.
    pub(crate) fn set_sp(&mut self, sp: usize) {
        self.sp = sp;
    }

    /// Construct the first EL0 entry context.
    pub(crate) fn app_init_context(
        entry: usize,
        sp: usize,
        _kernel_root: KernelTrapToken,
        kernel_sp: usize,
        trap_handler: UserTrapEntry,
    ) -> Self {
        let mut context = Self {
            x: [0; 31],
            sp,
            pc: entry,
            pstate: 0,
            kernel_ttbr: 0,
            kernel_sp,
            trap_handler: trap_handler.encoded(),
            kernel_cpu_id: 0,
            thread_pointer: 0,
        };
        context.set_sp(sp);
        context
    }

    /// Return the typed AArch64 illegal-instruction classification in a short context transaction.
    pub(crate) fn illegal_instruction_probe(&self) -> IllegalInstructionProbe {
        IllegalInstructionProbe::Fault(IllegalInstructionFault::new(self.pc))
    }

    /// Finish the typed AArch64 illegal-instruction result without a compatibility transition.
    pub(crate) fn finish_illegal_instruction(
        &mut self,
        result: Result<IllegalInstructionRetry, IllegalInstructionFault>,
    ) -> Result<(), IllegalInstructionFault> {
        match result {
            Err(fault) => {
                assert_eq!(fault.address(), self.pc, "illegal-instruction PC changed");
                Err(fault)
            }
            Ok(_) => panic!("AArch64 cannot request a lazy illegal-instruction retry"),
        }
    }
}

/// Resolve an AArch64 probe outside the context transaction.
pub(crate) fn inspect_illegal_instruction(
    probe: IllegalInstructionProbe,
    _read_halfword: impl FnMut(usize, &mut [u8]) -> bool,
) -> Result<IllegalInstructionRetry, IllegalInstructionFault> {
    match probe {
        IllegalInstructionProbe::Fault(fault) => Err(fault),
        IllegalInstructionProbe::Decode { .. } => {
            panic!("AArch64 produced an impossible illegal-instruction decode request")
        }
    }
}

/// 用户 syscall 指令携带的编号与参数。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SyscallRequest {
    pub(crate) number: usize,
    pub(crate) arguments: [usize; 7],
}

/// `NZCV` 是 EL0 唯一可写的 SPSR 位；其余位保持 EL0t、异常未屏蔽。
const USER_PSTATE_MASK: usize = 0xf000_0000;

impl UserContext {
    /// 读取 `x8`/`x0..x6`；`ELR_EL1` 已指向 `svc` 之后，返回时无需推进。
    pub(crate) fn take_syscall_request(&mut self) -> SyscallRequest {
        SyscallRequest {
            number: self.x[8],
            arguments: self.x[..7].try_into().expect("fixed syscall arguments"),
        }
    }

    /// 写回 syscall 返回值（`x0`）。
    pub(crate) fn set_syscall_result(&mut self, value: usize) {
        self.x[0] = value;
    }

    /// 用户 PC。
    pub(crate) fn program_counter(&self) -> usize {
        self.pc
    }

    /// 设置用户 PC。
    pub(crate) fn set_program_counter(&mut self, pc: usize) {
        self.pc = pc;
    }

    /// 设置首次进入用户态时的前两个参数寄存器（`x0`、`x1`）。
    pub(crate) fn set_entry_arguments(&mut self, first: usize, second: usize) {
        self.x[0] = first;
        self.x[1] = second;
    }

    /// 发布本次返回所在的逻辑 CPU，供 trampoline 恢复 kernel per-CPU 状态。
    pub(crate) fn prepare_kernel_return(&mut self, logical_cpu: usize) {
        self.kernel_cpu_id = logical_cpu;
    }

    /// 导出通用寄存器快照。
    pub(crate) fn save_registers(&self) -> native_abi::register::RegisterState {
        let mut x = [0; 31];
        for (target, source) in x.iter_mut().zip(self.x) {
            *target = source as u64;
        }
        native_abi::register::RegisterState {
            x,
            sp: self.sp as u64,
            pc: self.pc as u64,
            tpidr: self.thread_pointer as u64,
            nzcv: (self.pstate & USER_PSTATE_MASK) as u64,
        }
    }

    /// 以快照替换通用寄存器；只接受 `NZCV`，不允许快照改变异常级别或中断屏蔽。
    ///
    /// # Errors
    /// `nzcv` 含其他位或 PC/SP 越出用户地址空间时返回 `Err`，上下文保持不变。
    pub(crate) fn load_registers(
        &mut self,
        state: &native_abi::register::RegisterState,
    ) -> Result<(), ()> {
        let end = super::mmu::USER_ADDRESS_END as u64;
        if state.nzcv & !(USER_PSTATE_MASK as u64) != 0 || state.pc >= end || state.sp > end {
            return Err(());
        }
        for (target, source) in self.x.iter_mut().zip(state.x) {
            *target = source as usize;
        }
        self.sp = state.sp as usize;
        self.pc = state.pc as usize;
        self.thread_pointer = state.tpidr as usize;
        self.pstate = state.nzcv as usize;
        Ok(())
    }
}
