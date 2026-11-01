//! Linux/AArch64 用户执行状态。

pub(crate) mod fp;
pub(crate) mod signal_action;
pub(crate) mod signal_frame;
pub(crate) mod user;

use native_runtime::native_abi::{ExceptionReport, register::RegisterState};

/// `rt_sigreturn` 跳板：`mov x8, #139; svc #0`（Linux arm64 `__kernel_rt_sigreturn`）。
pub(crate) const SIGRETURN_TRAMPOLINE: [u32; 2] = [0xd280_1168, 0xd400_0001];

/// EL0 通用寄存器状态，以及最近一次投递的内存异常（信号帧的 `esr_context`/`fault_address`）。
#[derive(Debug, Clone)]
pub(crate) struct UserContext {
    pub(super) x: [usize; 31],
    pub(super) sp: usize,
    pub(super) pc: usize,
    /// 只有 NZCV（31..28 位）可由用户观察与修改。
    pub(super) pstate: usize,
    pub(super) thread_pointer: usize,
    // 本线程最后一次实际投递的内存异常，跟随 clone，exec 清零；缺失时用户态异常处理器
    // 无法从信号帧识别访问方向与类型。
    pub(super) fault_syndrome: u64,
    pub(super) fault_address: usize,
}

/// NZCV 之外的 PSTATE 位不属于用户状态。
pub(super) const PSTATE_USER_MASK: usize = 0xf000_0000;

impl UserContext {
    /// 新程序首次进入时的状态。
    pub(crate) fn new(entry: usize, stack: usize) -> Self {
        Self {
            x: [0; 31],
            sp: stack,
            pc: entry,
            pstate: 0,
            thread_pointer: 0,
            fault_syndrome: 0,
            fault_address: 0,
        }
    }

    /// 以 restricted 退出时保存的寄存器更新本上下文。
    pub(crate) fn load_registers(&mut self, state: &RegisterState) {
        for (register, value) in self.x.iter_mut().zip(state.x) {
            *register = value as usize;
        }
        self.sp = state.sp as usize;
        self.pc = state.pc as usize;
        self.thread_pointer = state.tpidr as usize;
        self.pstate = state.nzcv as usize & PSTATE_USER_MASK;
    }

    /// 进入 restricted mode 前写回的寄存器。
    pub(crate) fn registers(&self) -> RegisterState {
        let mut state = RegisterState::default();
        for (value, register) in state.x.iter_mut().zip(self.x) {
            *value = register as u64;
        }
        state.sp = self.sp as u64;
        state.pc = self.pc as u64;
        state.tpidr = self.thread_pointer as u64;
        state.nzcv = (self.pstate & PSTATE_USER_MASK) as u64;
        state
    }

    /// 记录被信号实际投递的内存异常。
    pub(crate) fn record_user_fault(&mut self, fault: UserFault) {
        self.fault_syndrome = fault.signal_syndrome();
        self.fault_address = fault.address();
    }
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

    /// 写入信号帧 `esr_context` 的综合码：Linux 只公开 EC、IL 与数据异常的 WnR/DFSC 位。
    pub(crate) fn signal_syndrome(&self) -> u64 {
        const EC_SHIFT: u64 = 26;
        const DATA_ABORT_LOWER: u64 = 0x24;
        let syndrome = self.report.syndrome;
        let class = (syndrome >> EC_SHIFT) & 0x3f;
        if class == DATA_ABORT_LOWER {
            // EC | IL | WnR | DFSC
            syndrome & ((0x3f << EC_SHIFT) | (1 << 25) | (1 << 6) | 0x3f)
        } else {
            syndrome & ((0x3f << EC_SHIFT) | (1 << 25))
        }
    }
}
