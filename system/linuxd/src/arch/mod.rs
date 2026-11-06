//! Linux 用户执行状态的架构部分：寄存器上下文、浮点状态、信号帧与 ELF/syscall 约定。
//!
//! Linux 程序在 restricted mode 运行；每次退出时 linuxd 从 restricted 状态读取通用寄存器构造
//! [`UserContext`]，返回前写回。浮点寄存器不在 restricted 状态内：服务代码不使用浮点，Linux
//! 程序的浮点状态始终留在硬件中，只在信号帧、clone、exec 与调试访问时经 [`fp`] 显式保存/恢复。

#[cfg(target_arch = "aarch64")]
#[path = "aarch64/mod.rs"]
mod selected;
#[cfg(target_arch = "riscv64")]
#[path = "riscv64/mod.rs"]
mod selected;

#[cfg(not(any(target_arch = "aarch64", target_arch = "riscv64")))]
compile_error!("linuxd has no implementation for this target architecture");

pub(crate) use selected::fp;
pub(crate) mod user_copy;

/// 用户寄存器上下文与 syscall 编解码。
pub(crate) mod context {
    pub(crate) use super::selected::signal_frame::{
        MIN_SIGNAL_STACK_SIZE, SIGNAL_FRAME_SIZE, SignalFrame, SignalStack,
    };
    pub(crate) use super::selected::user::SyscallCompletion;
    pub(crate) use super::selected::{UserContext, UserFault};
}

/// Linux 架构常量与 sigaction 布局。
pub(crate) mod user {
    pub(crate) use super::selected::SIGRETURN_TRAMPOLINE;
    pub(crate) use super::selected::signal_action::SignalActionBytes;
    pub(crate) use super::selected::user::{
        AUDIT_ARCH, ELF_HWCAP, ELF_MACHINE, MACHINE_NAME, decode_private_syscall, valid_elf_flags,
    };
}
