//! Linux/RISC-V 用户执行状态。

core::arch::global_asm!(include_str!("user_copy.S"));

pub(crate) mod fp;
pub(crate) mod signal_action;
pub(crate) mod signal_frame;
pub(crate) mod user;

use native_runtime::native_abi::{ExceptionReport, register::RegisterState};

/// `rt_sigreturn` 跳板：`li a7, 139; ecall`（Linux riscv vDSO `__vdso_rt_sigreturn`）。
pub(crate) const SIGRETURN_TRAMPOLINE: [u32; 2] = [0x08b0_0893, 0x0000_0073];

/// U-mode 通用寄存器状态。
#[derive(Debug, Clone)]
pub(crate) struct UserContext {
    /// x0..x31；`x[0]` 恒为 0，`x[2]` 为 sp，`x[4]` 为 tp。
    pub(super) x: [usize; 32],
    /// 用户 PC。
    pub(super) sepc: usize,
}

impl UserContext {
    /// 新程序首次进入时的状态。
    pub(crate) fn new(entry: usize, stack: usize) -> Self {
        let mut x = [0; 32];
        x[2] = stack;
        Self { x, sepc: entry }
    }

    /// 以 restricted 退出时保存的寄存器更新本上下文。
    pub(crate) fn load_registers(&mut self, state: &RegisterState) {
        self.sepc = state.pc as usize;
        for (register, value) in self.x[1..].iter_mut().zip(state.x) {
            *register = value as usize;
        }
    }

    /// 进入 restricted mode 前写回的寄存器。
    pub(crate) fn registers(&self) -> RegisterState {
        let mut state = RegisterState {
            pc: self.sepc as u64,
            ..RegisterState::default()
        };
        for (value, register) in state.x.iter_mut().zip(&self.x[1..]) {
            *value = *register as u64;
        }
        state
    }

    /// RISC-V 信号帧不含异常综合码；地址已由 siginfo 保存。
    pub(crate) fn record_user_fault(&mut self, _fault: UserFault) {}
}

/// 一次用户态异常（来自 restricted 退出的异常报告）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct UserFault {
    report: ExceptionReport,
}

impl UserFault {
    pub(crate) fn new(report: ExceptionReport) -> Self {
        Self { report }
    }

    /// 数据异常地址或指令 PC。
    pub(crate) fn address(&self) -> usize {
        self.report.address as usize
    }
}
