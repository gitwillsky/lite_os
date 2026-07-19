//! @description Linux/RISC-V 用户执行约定与 raw register context 之间的唯一转换。

use super::{
    UserContext,
    signal_frame::{SignalFrame, SignalMachineContext, SignalStack},
};

/// @description Linux utsname 使用的 architecture machine identity。
pub(crate) const MACHINE_NAME: &str = "riscv64";
pub(crate) const SUPPORTS_RISCV_HWPROBE: bool = true;
pub(crate) const ELF_MACHINE: u16 = 243;
pub(crate) const ELF_HWCAP: usize = (1 << 0)
    | (1 << (b'C' - b'A'))
    | (1 << (b'D' - b'A'))
    | (1 << (b'F' - b'A'))
    | (1 << (b'I' - b'A'))
    | (1 << (b'M' - b'A'));

/// @description 校验 Linux/RISC-V ELF header 的 architecture flags。
///
/// @param flags ELF64 header 的 `e_flags`。
/// @return flags 仅包含当前支持的 RVC/float ABI 编码且没有保留编码时返回 true。
pub(crate) const fn valid_elf_flags(flags: u32) -> bool {
    flags & !0x7 == 0 && flags & 0x6 != 0x6
}

/// @description 投影所有 online CPU 共同成立的保守 Linux RISC-V hwprobe value。
///
/// @param key Linux `RISCV_HWPROBE_KEY_*`。
/// @param time_counter_frequency platform time counter frequency。
/// @return 已知 key/value；未知 key 返回 `None`。
pub(crate) fn hardware_probe_value(key: i64, time_counter_frequency: u64) -> Option<u64> {
    const IMA: u64 = 1;
    const FD_AND_C: u64 = (1 << 0) | (1 << 1);
    const SV39_USER_ADDRESS_MAX: u64 = (1u64 << 38) - 1;
    match key {
        0..=2 => Some(0),
        3 => Some(IMA),
        4 => Some(FD_AND_C),
        5 | 6 | 9 | 11..=16 => Some(0),
        7 => Some(SV39_USER_ADDRESS_MAX),
        8 => Some(time_counter_frequency),
        10 => Some(4),
        _ => None,
    }
}

/// @description 已从 RISC-V user context 取出的完整 syscall request。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SyscallRequest {
    number: usize,
    arguments: [usize; 6],
    instruction: usize,
}

impl SyscallRequest {
    /// @description 获取 Linux/riscv64 syscall number。
    pub(crate) fn number(self) -> usize {
        self.number
    }

    /// @description 获取按 a0..a5 顺序保存的 syscall arguments。
    pub(crate) fn arguments(self) -> [usize; 6] {
        self.arguments
    }

    /// @description 获取触发 request 的 ecall instruction address。
    pub(crate) fn instruction(self) -> usize {
        self.instruction
    }
}

/// @description syscall dispatcher 对 user register state 的语义结果。
#[derive(Debug, Clone, Copy)]
pub(crate) enum SyscallCompletion {
    Return(isize),
    Interrupted(isize),
}

/// @description 用户提供的 signal machine context validation failure。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidSignalContext;

impl UserContext {
    /// @description 读取 syscall registers 并把 PC 推进到 ecall 之后。
    ///
    /// @return owned request；返回后 context 已准备写入 syscall completion。
    pub(crate) fn take_syscall_request(&mut self) -> SyscallRequest {
        let request = SyscallRequest {
            number: self.x[17],
            arguments: [
                self.x[10], self.x[11], self.x[12], self.x[13], self.x[14], self.x[15],
            ],
            instruction: self.sepc,
        };
        self.sepc = self
            .sepc
            .checked_add(4)
            .expect("ecall PC exhausted user address space");
        request
    }

    /// @description 将 syscall completion 写入 RISC-V return register。
    pub(crate) fn complete_syscall(&mut self, completion: SyscallCompletion) {
        self.x[10] = match completion {
            SyscallCompletion::Return(result) => result as usize,
            SyscallCompletion::Interrupted(result) => result as usize,
        };
    }

    /// @description 恢复一次被 signal 中断且需要重放的 ecall register state。
    pub(crate) fn restart_syscall(
        &mut self,
        number: usize,
        arguments: [usize; 6],
        instruction: usize,
    ) {
        assert_eq!(
            self.sepc,
            instruction
                .checked_add(4)
                .expect("restart ecall PC overflow"),
            "restart record does not match interrupted ecall"
        );
        self.x[10..16].copy_from_slice(&arguments);
        self.x[17] = number;
        self.sepc = instruction;
    }

    /// @description 获取用户 program counter。
    pub(crate) fn program_counter(&self) -> usize {
        self.sepc
    }

    /// @description 获取用户 stack pointer。
    pub(crate) fn stack_pointer(&self) -> usize {
        self.x[2]
    }

    /// @description 为 clone 创建的 child 准备首次 user return。
    pub(crate) fn prepare_thread_clone(
        &mut self,
        user_stack: usize,
        thread_pointer: usize,
        kernel_stack: usize,
    ) {
        self.x[2] = user_stack;
        self.x[4] = thread_pointer;
        self.prepare_child_return(kernel_stack);
    }

    /// @description 为 fork/vfork child 准备首次 user return。
    pub(crate) fn prepare_process_clone(&mut self, user_stack: Option<usize>, kernel_stack: usize) {
        if let Some(user_stack) = user_stack {
            self.x[2] = user_stack;
        }
        self.prepare_child_return(kernel_stack);
    }

    fn prepare_child_return(&mut self, kernel_stack: usize) {
        self.x[10] = 0;
        self.kernel_sp = kernel_stack;
        // 这些字段只由实际执行 child 的 CPU 在 user return 前重新发布；若继承 parent
        // 值，首次 trap 会恢复错误 CPU 的 tp/gp 并破坏 per-CPU state。
        self.kernel_cpu_id = 0;
        self.kernel_gp = 0;
    }

    /// @description 发布下一次 user trap entry 恢复 kernel tp/gp 所需的 CPU-local metadata。
    pub(crate) fn prepare_kernel_return(&mut self, logical_cpu: usize) {
        let kernel_gp: usize;
        // SAFETY: reading gp has no memory effect and preserves the kernel global-pointer value
        // required by the trampoline on the next supervisor entry.
        unsafe { core::arch::asm!("mv {}, gp", out(reg) kernel_gp, options(nomem, nostack)) };
        self.kernel_cpu_id = logical_cpu;
        self.kernel_gp = kernel_gp;
    }

    /// @description 编码 byte-for-byte 保持既有 ABI 的 Linux/RISC-V `rt_sigframe`。
    /// @param info 128-byte siginfo image。
    /// @param stack delivery 前的 alternate stack state。
    /// @param signal_mask delivery 前的 blocked mask。
    /// @return architecture-owned 1080-byte frame。
    pub(crate) fn capture_signal_frame(
        &self,
        info: [u8; 128],
        stack: SignalStack,
        signal_mask: u64,
    ) -> SignalFrame {
        let machine = self.capture_signal_machine_context();
        SignalFrame::encode(info, stack, signal_mask, &machine)
    }

    fn capture_signal_machine_context(&self) -> SignalMachineContext {
        let mut registers = [0usize; 32];
        registers[0] = self.sepc;
        registers[1..].copy_from_slice(&self.x[1..]);
        let mut floating_point = [0u8; 528];
        for (index, value) in self.f.iter().enumerate() {
            floating_point[index * 8..index * 8 + 8].copy_from_slice(&value.to_ne_bytes());
        }
        floating_point[256..260].copy_from_slice(&(self.fcsr as u32).to_ne_bytes());
        SignalMachineContext {
            registers,
            floating_point,
        }
    }

    /// @description 恢复经完整验证的 Linux signal machine context。
    ///
    /// @param machine 用户 frame 中的 owned machine context。
    /// @return 恢复后的 syscall result register a0。
    /// @errors unsupported extension header 非零时返回 `InvalidSignalContext`，context 不变。
    fn restore_signal_machine_context(
        &mut self,
        machine: &SignalMachineContext,
    ) -> Result<usize, InvalidSignalContext> {
        if machine.floating_point[516..528]
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(InvalidSignalContext);
        }

        self.sepc = machine.registers[0];
        self.x[1..].copy_from_slice(&machine.registers[1..]);
        for index in 0..32 {
            self.f[index] = u64::from_ne_bytes(
                machine.floating_point[index * 8..index * 8 + 8]
                    .try_into()
                    .expect("fixed signal FP register slice"),
            );
        }
        self.fcsr = u32::from_ne_bytes(
            machine.floating_point[256..260]
                .try_into()
                .expect("fixed signal FCSR slice"),
        ) as usize;
        // Signal frame 明确提供了完整 FP image；标为 Clean 使 restore path 在返回用户态前
        // 安装它。若保持 Off，sigreturn 会静默丢弃用户修改后的 FP 寄存器。
        self.sstatus.set_fs(riscv::register::sstatus::FS::Clean);
        Ok(self.x[10])
    }

    /// @description 解码并恢复既有 Linux/RISC-V signal frame。
    /// @param frame 从当前用户 SP 完整复制得到的 owned frame。
    /// @return `(a0, signal_mask, alternate_stack)`。
    /// @errors unsupported extension header 非零时 context 不变并返回错误。
    pub(crate) fn restore_signal_frame(
        &mut self,
        frame: &SignalFrame,
    ) -> Result<(usize, u64, SignalStack), InvalidSignalContext> {
        let decoded = frame.decode();
        let result = self.restore_signal_machine_context(&decoded.machine)?;
        Ok((result, decoded.signal_mask, decoded.signal_stack))
    }

    /// @description 安装 Linux/RISC-V signal handler entry register state。
    pub(crate) fn enter_signal_handler(
        &mut self,
        trampoline: usize,
        frame: usize,
        signal: usize,
        handler: usize,
    ) {
        self.x[1] = trampoline;
        self.x[2] = frame;
        self.x[10] = signal;
        self.x[11] = frame;
        self.x[12] = frame + 128;
        self.sepc = handler;
    }
}
