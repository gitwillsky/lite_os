#![no_std]

//! 系统服务运行时：native 调用封装、进程入口、堆、线程、同步原语与 async executor。
//!
//! 只服务于系统服务（根服务、linuxd、驱动与文件系统服务）；应用不链接本 crate。
//!
//! # 进程模型
//!
//! 1. 进程从 `_start` 进入，读取启动消息（`native_abi::startup`），用 `SELF_VMAR` 安装堆、
//!    用 `SELF_THREAD` 安装主线程描述符，再调用服务以 [`entry!`] 定义的 `main`。
//! 2. 服务代码按无浮点 ABI 编译（AArch64 softfloat、RISC-V `imac`），从不触碰浮点寄存器：
//!    在同一线程上运行的 restricted 代码的浮点状态因此跨退出保持原样。

extern crate alloc;

pub mod bootfs;
pub mod elf;
pub mod executor;
pub mod futex;
mod handle;
mod hardware;
pub mod heap;
mod ipc;
pub mod launch;
pub mod log;
mod memory;
pub mod restricted;
mod startup;
pub mod sync;
pub mod syscall;
pub mod system;
pub mod task;
pub mod thread;

pub use handle::Handle;
pub use hardware::{Bti, Interrupt, Iommu, Pmt, Resource};
pub use ipc::{Channel, Event, Message, Port};
pub use memory::{Pager, Vmar, Vmo};
pub use native_abi;
pub use startup::{Startup, self_process, set_fatal_scope};
pub use task::{Exception, Job, Process, Thread};

/// 可按字节复制的纯数据类型（`repr(C)`、无填充敏感不变量、全零合法）。
///
/// # Safety
///
/// 实现者的任何字节模式都必须是合法值，且不含指针或引用。
pub unsafe trait Plain: Copy + Default {}

macro_rules! plain {
    ($($type:ty),* $(,)?) => {
        $(
            // SAFETY: 这些都是只含整数字段的 `repr(C)` 结构或整数。
            unsafe impl Plain for $type {}
        )*
    };
}

plain!(
    u8,
    u16,
    u32,
    u64,
    usize,
    i32,
    i64,
    native_abi::HandleBasic,
    native_abi::TaskRuntime,
    native_abi::TaskStats,
    native_abi::VmarRegion,
    native_abi::VmoInfo,
    native_abi::ProcessBasic,
    native_abi::SystemBasic,
    native_abi::StartupHeader,
    native_abi::ExceptionReport,
    native_abi::RestrictedState,
    native_abi::PortPacket,
    native_abi::register::RegisterState,
);

/// 值的字节视图。
pub fn bytes_of<T: Plain>(value: &T) -> &[u8] {
    // SAFETY: `Plain` 保证按字节读取合法。
    unsafe {
        core::slice::from_raw_parts(core::ptr::from_ref(value).cast(), core::mem::size_of::<T>())
    }
}

/// 值的可写字节视图。
pub fn bytes_of_mut<T: Plain>(value: &mut T) -> &mut [u8] {
    // SAFETY: `Plain` 保证任意字节模式合法。
    unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::from_mut(value).cast(),
            core::mem::size_of::<T>(),
        )
    }
}
