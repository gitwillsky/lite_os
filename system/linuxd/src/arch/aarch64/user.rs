//! Linux/AArch64 user register conventions.

use super::{
    UserContext, fp,
    signal_frame::{InvalidSignalFrame, SignalFrame, SignalStack},
};
pub(crate) const MACHINE_NAME: &str = "aarch64";
/// Linux AUDIT_ARCH_AARCH64：64-bit、little-endian、ELF machine 183。
pub(crate) const AUDIT_ARCH: u32 = 0xc000_00b7;
/// Decode an AArch64-private Linux syscall number.
///
/// AArch64 currently owns no private syscall numbers; the compile-time façade lets release builds
/// erase this call and its architecture-dispatch branch completely.
pub(crate) const fn decode_private_syscall(_syscall_id: usize) -> Option<usize> {
    None
}
pub(crate) const ELF_MACHINE: u16 = 183;
/// Linux arm64 baseline FP/Advanced SIMD capability bits exposed to userspace.
pub(crate) const ELF_HWCAP: usize = (1 << 0) | (1 << 1);

/// 校验 Linux/AArch64 ELF header 的 architecture flags。
///
/// # Parameters
///
/// - `flags`: ELF64 header 的 `e_flags`。
///
/// # Returns
///
/// AArch64 保留字段为零时返回 true。
pub(crate) const fn valid_elf_flags(flags: u32) -> bool {
    flags == 0
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SyscallRequest {
    number: usize,
    arguments: [usize; 6],
    instruction: usize,
}

impl SyscallRequest {
    pub(crate) fn number(self) -> usize {
        self.number
    }
    pub(crate) fn arguments(self) -> [usize; 6] {
        self.arguments
    }
    pub(crate) fn instruction(self) -> usize {
        self.instruction
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SyscallCompletion {
    Return(isize),
    Interrupted(isize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidSignalContext;

impl From<InvalidSignalFrame> for InvalidSignalContext {
    fn from(_: InvalidSignalFrame) -> Self {
        Self
    }
}

impl UserContext {
    /// Decode x8/x0..x5 and advance past the 4-byte SVC instruction.
    pub(crate) fn take_syscall_request(&mut self) -> SyscallRequest {
        let instruction = self.pc.checked_sub(4).expect("SVC return PC underflow");
        SyscallRequest {
            number: self.x[8],
            arguments: self.x[..6].try_into().expect("fixed syscall arguments"),
            instruction,
        }
    }

    pub(crate) fn complete_syscall(&mut self, completion: SyscallCompletion) {
        self.x[0] = match completion {
            SyscallCompletion::Return(value) | SyscallCompletion::Interrupted(value) => {
                value as usize
            }
        };
    }

    pub(crate) fn restart_syscall(
        &mut self,
        number: usize,
        arguments: [usize; 6],
        instruction: usize,
    ) {
        assert_eq!(
            self.pc,
            instruction.checked_add(4).expect("restart SVC PC overflow")
        );
        self.x[..6].copy_from_slice(&arguments);
        self.x[8] = number;
        self.pc = instruction;
    }

    pub(crate) fn program_counter(&self) -> usize {
        self.pc
    }
    pub(crate) fn stack_pointer(&self) -> usize {
        self.sp
    }

    /// 为 clone 创建的 child 准备首次返回：返回值 0，可选新栈与 TLS。
    pub(crate) fn prepare_clone(&mut self, user_stack: Option<usize>, thread_pointer: Option<usize>) {
        if let Some(stack) = user_stack {
            self.sp = stack;
        }
        if let Some(tls) = thread_pointer {
            self.thread_pointer = tls;
        }
        self.x[0] = 0;
    }

    /// 编码标准 Linux/arm64 `rt_sigframe` 并捕获 live FPSIMD state。
    ///
    /// # Parameters
    ///
    /// - `info`: 128-byte siginfo image。
    /// - `stack`: delivery 前的 alternate stack state。
    /// - `signal_mask`: delivery 前的 blocked mask。
    ///
    /// # Returns
    ///
    /// 4688-byte architecture-owned frame。
    pub(crate) fn capture_signal_frame(
        &self,
        info: [u8; 128],
        stack: SignalStack,
        signal_mask: u64,
    ) -> SignalFrame {
        let mut frame = SignalFrame::encode(
            info,
            stack,
            signal_mask,
            self.x,
            self.sp,
            self.pc,
            self.pstate,
        );
        frame.set_fault_context(self.fault_address, self.fault_syndrome);
        // SAFETY: frame 16 字节对齐且由本函数独占；例程只写 FPSIMD 记录体。
        unsafe { fp::capture_signal(frame.fpsimd_state_mut_ptr()) };
        frame
    }

    /// 验证并恢复 Linux/arm64 integer 与 live FPSIMD signal state。
    ///
    /// # Parameters
    ///
    /// - `frame`: 从当前用户 SP 完整复制得到的 owned frame。
    ///
    /// # Returns
    ///
    /// `(x0, signal_mask, alternate_stack)`。
    ///
    /// # Errors
    ///
    /// context chain、EL0 PSTATE 或用户 PC 非法时 context/live FP 均保持不变。
    pub(crate) fn restore_signal_frame(
        &mut self,
        frame: &SignalFrame,
    ) -> Result<(usize, u64, SignalStack), InvalidSignalContext> {
        let decoded = frame.decode(crate::memory::user_address_end())?;
        // SAFETY: decode 已证明存在一个完整、对齐的 FPSIMD 记录；frame 在恢复期间保持不变。
        unsafe { fp::restore_signal(frame.fpsimd_state_ptr(&decoded)) };
        self.x = decoded.registers;
        self.sp = decoded.stack_pointer;
        self.pc = decoded.program_counter;
        self.pstate = decoded.pstate;
        Ok((self.x[0], decoded.signal_mask, decoded.signal_stack))
    }

    pub(crate) fn enter_signal_handler(
        &mut self,
        trampoline: usize,
        frame: usize,
        signal: usize,
        handler: usize,
        flags: usize,
        restorer: usize,
    ) {
        const SA_RESTORER: usize = 0x0400_0000;
        self.x[30] = if flags & SA_RESTORER != 0 {
            restorer
        } else {
            trampoline
        };
        self.sp = frame;
        self.x[0] = signal;
        self.x[1] = frame;
        self.x[2] = frame + 128;
        self.pc = handler;
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
            // Linux valid_user_regs: EL0t、未屏蔽 DAIF；restricted 状态只承载 NZCV，其余用户位不保留。
            const USER_PSTATE: usize = 0xf000_1c00;
            if words[33] & !USER_PSTATE != 0 {
                return Err(());
            }
            self.x.copy_from_slice(&words[..31]);
            self.sp = words[31];
            self.pc = words[32];
            self.pstate = words[33] & super::PSTATE_USER_MASK;
        }
        Ok(length)
    }

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
