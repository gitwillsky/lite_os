use super::mmu::KernelTrapToken;
use super::trap::UserTrapEntry;
use crate::arch::{IllegalInstructionFault, IllegalInstructionProbe, IllegalInstructionRetry};
use riscv::register::sstatus::{self, SPP, Sstatus};

/// RISC-V UserContext 必须保留在 trampoline 可访问的 TTBR0/Sv39 映射中。
pub(crate) const KERNEL_STACK_CONTEXT_RESERVE: usize = 0;
/// RISC-V keeps the U-mode register image in its supervisor trap-context mapping.
pub(crate) const USER_CONTEXT_PLACEMENT: crate::arch::UserContextPlacement =
    crate::arch::UserContextPlacement::AddressSpace;

/// U-mode 与 S-mode trap 路径之间共享的完整用户执行上下文。
#[repr(C)]
#[derive(Debug, Clone)]
pub(crate) struct UserContext {
    /// 用户通用寄存器 x0..x31，包括 psABI 的 gp(x3) 与 tp(x4)。
    pub(super) x: [usize; 32],
    /// trap 发生时的 supervisor status。
    pub(super) sstatus: Sstatus,
    /// trap 返回的用户程序计数器。
    pub(super) sepc: usize,
    /// trampoline 进入内核时切换的 kernel satp。
    pub(super) kernel_satp: usize,
    /// 当前任务的内核栈顶。
    pub(super) kernel_sp: usize,
    /// S-mode Rust trap handler 入口。
    pub(super) trap_handler: usize,
    /// 当前执行该任务的 hart ID，用于在保存用户 tp 后恢复 kernel tp。
    pub(super) kernel_cpu_id: usize,
    /// kernel psABI global pointer，用于在保存用户 gp 后恢复 kernel gp。
    pub(super) kernel_gp: usize,
    /// 用户浮点寄存器 f0..f31 的原始 64-bit 内容。
    pub(super) f: [u64; 32],
    /// 用户 floating-point control/status register。
    pub(super) fcsr: usize,
}

const _: () = {
    use core::mem::{offset_of, size_of};
    const WORD: usize = size_of::<usize>();
    assert!(offset_of!(UserContext, sstatus) == 32 * WORD);
    assert!(offset_of!(UserContext, kernel_satp) == 34 * WORD);
    assert!(offset_of!(UserContext, kernel_cpu_id) == 37 * WORD);
    assert!(offset_of!(UserContext, kernel_gp) == 38 * WORD);
    assert!(offset_of!(UserContext, f) == 39 * WORD);
    assert!(offset_of!(UserContext, fcsr) == 71 * WORD);
    assert!(size_of::<UserContext>() == 72 * WORD);
};

impl UserContext {
    /// 设置用户栈指针。
    ///
    /// # Parameters
    ///
    /// - `sp`: 满足用户 ABI 对齐要求的栈顶。
    pub(crate) fn set_sp(&mut self, sp: usize) {
        self.x[2] = sp;
    }

    /// 构造首次进入用户程序所需的 trap context。
    ///
    /// # Parameters
    ///
    /// - `entry`: ELF 入口虚拟地址。
    /// - `sp`: 用户初始栈指针。
    /// - `kernel_satp`: kernel 页表 token。
    /// - `kernel_sp`: 当前任务内核栈顶。
    /// - `trap_handler`: S-mode trap handler 地址。
    ///
    /// # Returns
    ///
    /// 通用寄存器、浮点寄存器和 fcsr 均为零的初始上下文。
    pub(crate) fn app_init_context(
        entry: usize,
        sp: usize,
        kernel_satp: KernelTrapToken,
        kernel_sp: usize,
        trap_handler: UserTrapEntry,
    ) -> Self {
        let mut sstatus = sstatus::read(); // CSR status
        sstatus.set_spp(SPP::User);
        // SIE 必须在 S 模式恢复阶段保持关闭，否则切换到用户页表后会在内核栈失去映射时被中断。
        sstatus.set_sie(false);
        // SPIE 控制 sret 后的中断状态，确保进入用户态后可以正常响应中断。
        sstatus.set_spie(true);
        sstatus.set_sum(false);
        sstatus.set_mxr(false);
        // 首次 FP 指令由 FS=Off 精确触发 lazy activation；缺失该状态会让纯整数任务
        // 也在每次 trap 保存/恢复 32 个浮点寄存器。
        sstatus.set_fs(sstatus::FS::Off);

        let mut cx = Self {
            x: [0; 32],
            sstatus,
            sepc: entry,
            kernel_satp: kernel_satp.encoded(),
            kernel_sp,
            trap_handler: trap_handler.encoded(),
            kernel_cpu_id: 0,
            kernel_gp: 0,
            f: [0; 32],
            fcsr: 0,
        };

        cx.set_sp(sp);
        cx
    }

    /// 在短 context transaction 中分类 illegal instruction。
    ///
    /// # Returns
    ///
    /// `FS=Off` 时请求 transaction 外解码；其他状态直接报告非法指令异常。
    pub(crate) fn illegal_instruction_probe(&self) -> IllegalInstructionProbe {
        let address = self.sepc;
        if self.sstatus.fs() == sstatus::FS::Off {
            IllegalInstructionProbe::Decode { address }
        } else {
            IllegalInstructionProbe::Fault(IllegalInstructionFault::new(address))
        }
    }

    /// 在第二个短 transaction 中提交已验证的 lazy-FP transition。
    pub(crate) fn finish_illegal_instruction(
        &mut self,
        result: Result<IllegalInstructionRetry, IllegalInstructionFault>,
    ) -> Result<(), IllegalInstructionFault> {
        match result {
            Err(fault) => Err(fault),
            Ok(retry) => {
                assert_eq!(retry.address(), self.sepc, "illegal-instruction PC changed");
                assert_eq!(
                    self.sstatus.fs(),
                    sstatus::FS::Off,
                    "illegal-instruction FP state changed"
                );
                self.f.fill(0);
                self.fcsr = 0;
                self.sstatus.set_fs(sstatus::FS::Initial);
                Ok(())
            }
        }
    }
}

/// 在 ContextOwner transaction 外执行可能取得 AddressSpace lock 的指令读取。
pub(crate) fn inspect_illegal_instruction(
    probe: IllegalInstructionProbe,
    read_halfword: impl FnMut(usize, &mut [u8]) -> bool,
) -> Result<IllegalInstructionRetry, IllegalInstructionFault> {
    match probe {
        IllegalInstructionProbe::Fault(fault) => Err(fault),
        IllegalInstructionProbe::Decode { address } => {
            if super::fp_instruction::is_floating_point_instruction_at(address, read_halfword) {
                Ok(IllegalInstructionRetry::new(address))
            } else {
                Err(IllegalInstructionFault::new(address))
            }
        }
    }
}

/// 用户 syscall 指令携带的编号与参数。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SyscallRequest {
    pub(crate) number: usize,
    pub(crate) arguments: [usize; 7],
}

impl UserContext {
    /// 读取 `a7`/`a0..a6` 并把 `sepc` 推进到 `ecall` 之后。
    pub(crate) fn take_syscall_request(&mut self) -> SyscallRequest {
        let request = SyscallRequest {
            number: self.x[17],
            arguments: [
                self.x[10], self.x[11], self.x[12], self.x[13], self.x[14], self.x[15], self.x[16],
            ],
        };
        self.sepc = self
            .sepc
            .checked_add(4)
            .expect("ecall PC exhausted user address space");
        request
    }

    /// 写回 syscall 返回值（`a0`）。
    pub(crate) fn set_syscall_result(&mut self, value: usize) {
        self.x[10] = value;
    }

    /// 用户 PC。
    pub(crate) fn program_counter(&self) -> usize {
        self.sepc
    }

    /// 设置用户 PC。
    pub(crate) fn set_program_counter(&mut self, pc: usize) {
        self.sepc = pc;
    }

    /// 设置首次进入用户态时的前两个参数寄存器（`a0`、`a1`）。
    pub(crate) fn set_entry_arguments(&mut self, first: usize, second: usize) {
        self.x[10] = first;
        self.x[11] = second;
    }

    /// 发布本次返回所在的逻辑 CPU 与 kernel `gp`，供 trampoline 在下次进入时恢复。
    pub(crate) fn prepare_kernel_return(&mut self, logical_cpu: usize) {
        let kernel_gp: usize;
        // SAFETY: reading gp has no memory effect and preserves the kernel global-pointer value
        // required by the trampoline on the next supervisor entry.
        unsafe { core::arch::asm!("mv {}, gp", out(reg) kernel_gp, options(nomem, nostack)) };
        self.kernel_cpu_id = logical_cpu;
        self.kernel_gp = kernel_gp;
    }

    /// 导出通用寄存器快照。
    pub(crate) fn save_registers(&self) -> native_abi::register::RegisterState {
        let mut x = [0; 31];
        for (target, source) in x.iter_mut().zip(&self.x[1..]) {
            *target = *source as u64;
        }
        native_abi::register::RegisterState {
            pc: self.sepc as u64,
            x,
        }
    }

    /// 以快照替换通用寄存器；`x0` 恒为 0，`sstatus` 不受快照影响。
    ///
    /// # Errors
    /// PC 越出用户地址空间时返回 `Err`，上下文保持不变。
    pub(crate) fn load_registers(
        &mut self,
        state: &native_abi::register::RegisterState,
    ) -> Result<(), ()> {
        if state.pc >= super::mmu::USER_ADDRESS_END as u64 {
            return Err(());
        }
        self.sepc = state.pc as usize;
        for (target, source) in self.x[1..].iter_mut().zip(state.x) {
            *target = source as usize;
        }
        Ok(())
    }
}
