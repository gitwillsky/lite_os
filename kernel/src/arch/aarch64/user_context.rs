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
    // 本 Thread 最后一次实际投递的内存 fault，跟随 clone，exec 重新清零；不从当前 CPU 的易失 ESR 回读。
    // 缺少它会在 migration 后丢失异常方向/类型，用户态异常处理器无法识别未对齐访问。
    pub(super) fault_syndrome: u64,
    pub(super) fault_address: usize,
}

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(offset_of!(UserContext, sp) == 248);
    assert!(offset_of!(UserContext, kernel_ttbr) == 272);
    assert!(size_of::<UserContext>() == 336);
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
    /// 保存被 signal owner 实际投递的内存异常。
    ///
    /// # Parameters
    /// - `fault`: 同一次 trap 的不可变架构快照。
    ///
    /// # Returns
    /// 无返回值；更新本 Thread 的 signal diagnostic metadata。
    pub(crate) fn record_user_fault(&mut self, fault: super::trap::UserFault) {
        self.fault_syndrome = fault.signal_syndrome();
        self.fault_address = fault.address();
    }

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
            fault_syndrome: 0,
            fault_address: 0,
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

impl UserContext {
    /// 编解码 Linux NT_PRSTATUS；只暴露 EL0 通用寄存器，不允许改写 kernel metadata。
    ///
    /// # Parameters
    /// - `note`: ELF note，当前支持 NT_PRSTATUS。
    /// - `bytes`: 长度为八字节倍数的 kernel staging，可传标准 regset 的前缀。
    /// - `write`: true 表示以原状态补足前缀后原子校验、提交。
    ///
    /// # Returns
    /// 实际传输字节数，不超过 272。
    ///
    /// # Errors
    /// 未实现 note、非 word 大小或包含特权 PSTATE 的写入返回 Err。
    pub(crate) fn ptrace_registers(
        &mut self,
        note: usize,
        bytes: &mut [u8],
        write: bool,
    ) -> Result<usize, ()> {
        if note != syscall_abi::ptrace::NT_PRSTATUS || !bytes.len().is_multiple_of(8) {
            return Err(());
        }
        let length = bytes.len().min(272);
        let mut words = [0usize; 34];
        words[..31].copy_from_slice(&self.x);
        words[31] = self.sp;
        words[32] = self.pc;
        words[33] = self.pstate;
        for (word, chunk) in words.iter_mut().zip(bytes[..length].as_chunks_mut::<8>().0) {
            if write {
                *word = usize::from_ne_bytes(*chunk);
            } else {
                chunk.copy_from_slice(&word.to_ne_bytes());
            }
        }
        if write {
            // Linux valid_user_regs: EL0t、未屏蔽 DAIF；不把调试输入作为 kernel SPSR 恢复。
            const USER_PSTATE: usize = 0xf000_1c00;
            if words[33] & !USER_PSTATE != 0 {
                return Err(());
            }
            self.x.copy_from_slice(&words[..31]);
            self.sp = words[31];
            self.pc = words[32];
            self.pstate = words[33];
        }
        Ok(length)
    }
}

impl UserContext {
    /// 完成 tracer 已观察的 restart；PC 被 debugger 修改时不再覆盖它。
    ///
    /// # Parameters
    /// - `instruction`: 原 syscall 指令 PC。
    /// - `replay_number`: None 恢复 EINTR；Some 保留 tracer 参数，仅 restart-block 选择原生入口。
    ///
    /// # Returns
    /// 无返回值；仅当前 PC 仍在原指令时完成正常的 restart/handler 决议。
    pub(crate) fn finish_ptrace_restart(
        &mut self,
        instruction: usize,
        replay_number: Option<usize>,
    ) {
        if self.pc != instruction {
            return;
        }
        match replay_number {
            None => {
                self.pc += 4;
                self.x[0] = (-syscall_abi::errno::EINTR) as usize;
            }
            Some(syscall_abi::SYSCALL_RESTART_SYSCALL) => {
                self.x[8] = syscall_abi::SYSCALL_RESTART_SYSCALL
            }
            Some(_) => {}
        }
    }
}
