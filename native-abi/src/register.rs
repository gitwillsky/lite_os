//! 用户态通用寄存器快照：restricted mode 状态与异常报告共用的唯一布局。
//!
//! 浮点/向量寄存器不在快照内：normal 与 restricted 两种模式共享同一套浮点状态，
//! 需要隔离的一方自行保存（与 Zircon restricted mode 相同）。

/// AArch64 EL0 通用寄存器。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Aarch64RegisterState {
    /// `x0..x30`。
    pub x: [u64; 31],
    /// `SP_EL0`。
    pub sp: u64,
    /// 返回地址（`ELR_EL1`）。
    pub pc: u64,
    /// `TPIDR_EL0`。
    pub tpidr: u64,
    /// 条件标志 `NZCV`（`SPSR_EL1` 的 31..28 位）；其余位必须为 0。
    pub nzcv: u64,
}

/// RISC-V64 U-mode 通用寄存器。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Riscv64RegisterState {
    /// 返回地址（`sepc`）。
    pub pc: u64,
    /// `x1..x31`（`x[0]` 对应 `x1`/`ra`，`x[3]` 对应 `x4`/`tp`）。
    pub x: [u64; 31],
}

/// 当前目标架构的寄存器快照。
#[cfg(target_arch = "aarch64")]
pub type RegisterState = Aarch64RegisterState;
/// 当前目标架构的寄存器快照。
#[cfg(target_arch = "riscv64")]
pub type RegisterState = Riscv64RegisterState;
