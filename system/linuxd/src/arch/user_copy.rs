//! 当前成员地址空间的可恢复字节复制。架构例程是叶函数，异常入口无需展开栈。

use native_runtime::native_abi::Status;

// SAFETY: 两个后端定义同一 C ABI 的叶函数，结束及恢复符号只用于取地址。
unsafe extern "C" {
    fn linuxd_user_copy(dst: *mut u8, src: *const u8, len: usize, to_user: usize) -> usize;
    static linuxd_user_copy_end: u8;
    static linuxd_user_copy_fault: u8;
}

/// 为当前 Linux 承载线程启用复制例程的异常恢复。
///
/// # Errors
///
/// 当前线程不属于共享进程成员或已登记。
pub(crate) fn register() -> Result<(), Status> {
    // SAFETY: 两个后端仅在此叶函数中访问参数指向的内存。故障入口直接返回失败，
    // 原栈与返回地址均未改动，三个地址在不可变的共享映像中。
    unsafe {
        native_runtime::restricted::register_fault_recovery(
            linuxd_user_copy as *const () as usize,
            &raw const linuxd_user_copy_end as usize,
            &raw const linuxd_user_copy_fault as usize,
        )
    }
}

/// 复制 `len` 字节，完整复制返回 true；不可恢复的应用地址故障可留下已写入的前缀。
///
/// # Safety
///
/// 调用线程必须已登记恢复入口并运行在目标应用的地址空间。对应的应用区间已校验
/// 且映射受调用方锁保护；另一侧是可访问的服务存储。两个区间不得重叠。
pub(crate) unsafe fn copy(dst: *mut u8, src: *const u8, len: usize, to_user: bool) -> bool {
    // SAFETY: 由调用者保证上述地址空间、存储与映射条件。外部例程不创建 Rust 引用。
    unsafe { linuxd_user_copy(dst, src, len, usize::from(to_user)) == 0 }
}
