//! Linux/AArch64 user register conventions.

use super::{
    UserContext, fp_state,
    mmu::USER_ADDRESS_END,
    signal_frame::{InvalidSignalFrame, SignalFrame, SignalStack},
};
pub(crate) const MACHINE_NAME: &str = "aarch64";
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

/// @description 校验 Linux/AArch64 ELF header 的 architecture flags。
///
/// @param flags ELF64 header 的 `e_flags`。
/// @return AArch64 保留字段为零时返回 true。
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

    pub(crate) fn prepare_thread_clone(
        &mut self,
        user_stack: usize,
        thread_pointer: usize,
        kernel_stack: usize,
    ) {
        self.sp = user_stack;
        self.thread_pointer = thread_pointer;
        self.prepare_child_return(kernel_stack);
    }

    pub(crate) fn prepare_process_clone(&mut self, user_stack: Option<usize>, kernel_stack: usize) {
        if let Some(stack) = user_stack {
            self.sp = stack;
        }
        self.prepare_child_return(kernel_stack);
    }

    fn prepare_child_return(&mut self, kernel_stack: usize) {
        self.x[0] = 0;
        self.kernel_sp = kernel_stack;
        self.kernel_cpu_id = 0;
    }

    pub(crate) fn prepare_kernel_return(&mut self, logical_cpu: usize) {
        self.kernel_cpu_id = logical_cpu;
    }

    /// @description 编码标准 Linux/arm64 `rt_sigframe` 并捕获 live FPSIMD state。
    /// @param info 128-byte siginfo image。
    /// @param stack delivery 前的 alternate stack state。
    /// @param signal_mask delivery 前的 blocked mask。
    /// @return 4688-byte architecture-owned frame。
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
        // SAFETY: frame is 16-byte aligned and uniquely owned; helper writes exactly the FPSIMD
        // body, temporarily opens FPEN, and closes it before returning to Rust.
        unsafe { fp_state::capture_signal(frame.fpsimd_state_mut_ptr()) };
        frame
    }

    /// @description 验证并恢复 Linux/arm64 integer 与 live FPSIMD signal state。
    /// @param frame 从当前用户 SP 完整复制得到的 owned frame。
    /// @return `(x0, signal_mask, alternate_stack)`。
    /// @errors context chain、EL0 PSTATE 或用户 PC 非法时 context/live FP 均保持不变。
    pub(crate) fn restore_signal_frame(
        &mut self,
        frame: &SignalFrame,
    ) -> Result<(usize, u64, SignalStack), InvalidSignalContext> {
        let decoded = frame.decode(USER_ADDRESS_END)?;
        // SAFETY: decode proved one complete, aligned FPSIMD record and rejected malformed trailing
        // state; frame remains immutable/live while assembly restores the calling CPU's vector file.
        unsafe { fp_state::restore_signal(frame.fpsimd_state_ptr()) };
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
    ) {
        self.x[30] = trampoline;
        self.sp = frame;
        self.x[0] = signal;
        self.x[1] = frame;
        self.x[2] = frame + 128;
        self.pc = handler;
    }
}
