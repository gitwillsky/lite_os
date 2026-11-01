//! 时钟、日志、随机数与系统信息。

use native_abi::{DebuglogRecord, Status, SystemBasic, call};

use crate::Resource;
use crate::syscall::{check, invoke};

/// 单调时钟纳秒。
pub fn monotonic() -> i64 {
    let mut now = 0i64;
    // SAFETY: 输出位置有效；该调用不会失败。
    unsafe { invoke(call::CLOCK_GET_MONOTONIC, &[(&raw mut now) as usize]) };
    now
}

/// UTC 纳秒。
pub fn utc() -> i64 {
    let mut now = 0i64;
    // SAFETY: 输出位置有效；该调用不会失败。
    unsafe { invoke(call::CLOCK_GET_UTC, &[(&raw mut now) as usize]) };
    now
}

/// 设置 UTC。
///
/// # Errors
///
/// 不是根 resource 或时间早于本次启动。
pub fn set_utc(resource: &Resource, utc_ns: i64) -> Result<(), Status> {
    // SAFETY: 无指针参数。
    check(unsafe { invoke(call::CLOCK_SET_UTC, &[resource.raw() as usize, utc_ns as usize]) })
}

/// 睡眠到截止时间。
///
/// # Errors
///
/// `ERR_CANCELED`（线程被终止）。
pub fn sleep_until(deadline: i64) -> Result<(), Status> {
    // SAFETY: 无指针参数。
    check(unsafe { invoke(call::NANOSLEEP, &[deadline as usize]) })
}

/// 把纳秒时长换算为从现在起的截止时间（饱和）。
pub fn deadline_after(nanoseconds: i64) -> i64 {
    monotonic().saturating_add(nanoseconds)
}

/// 日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Severity {
    Debug = 0,
    Info = 1,
    Warning = 2,
    Error = 3,
}

/// 向内核日志写入一条记录（超出上限截断）。
pub fn debuglog_write(severity: Severity, message: &[u8]) {
    // SAFETY: 消息缓冲区有效；写日志失败没有可恢复动作。
    let _ = unsafe {
        invoke(
            call::DEBUGLOG_WRITE,
            &[severity as usize, message.as_ptr() as usize, message.len()],
        )
    };
}

/// 按序号读取内核日志。
///
/// # Errors
///
/// `ERR_SHOULD_WAIT`、`ERR_OUT_OF_RANGE`（记录头写回最老可读序号）、`ERR_TIMED_OUT`。
pub fn debuglog_read(resource: &Resource, sequence: u64, record: &mut DebuglogRecord, deadline: i64) -> Result<(), Status> {
    // SAFETY: 记录缓冲区有效。
    check(unsafe {
        invoke(
            call::DEBUGLOG_READ,
            &[resource.raw() as usize, sequence as usize, core::ptr::from_mut(record) as usize, deadline as usize],
        )
    })
}

/// 读取调试串口输入，返回字节数。
///
/// # Errors
///
/// `ERR_TIMED_OUT`、`ERR_SHOULD_WAIT`。
pub fn serial_read(resource: &Resource, buffer: &mut [u8], deadline: i64) -> Result<usize, Status> {
    let mut actual = 0usize;
    // SAFETY: 缓冲区与输出位置有效。
    check(unsafe {
        invoke(
            call::SERIAL_READ,
            &[
                resource.raw() as usize,
                buffer.as_mut_ptr() as usize,
                buffer.len(),
                deadline as usize,
                (&raw mut actual) as usize,
            ],
        )
    })?;
    Ok(actual)
}

/// 调用线程当前所在的逻辑 CPU（只作提示：返回时可能已迁移）。
pub fn current_cpu() -> u32 {
    let mut cpu = 0u32;
    // SAFETY: 输出位置有效；该调用不会失败。
    unsafe { invoke(call::CPU_CURRENT, &[(&raw mut cpu) as usize]) };
    cpu
}

/// 原样写调试串口。
///
/// # Errors
///
/// 不是根 resource。
pub fn serial_write(resource: &Resource, bytes: &[u8]) -> Result<(), Status> {
    // SAFETY: 缓冲区有效。
    check(unsafe { invoke(call::SERIAL_WRITE, &[resource.raw() as usize, bytes.as_ptr() as usize, bytes.len()]) })
}

/// 让每个 CPU 执行一次完整内存屏障后返回。
pub fn barrier() {
    // SAFETY: 无参数；该调用不会失败。
    unsafe { invoke(call::SYSTEM_BARRIER, &[]) };
}

/// 填充密码学安全随机字节。
pub fn cprng_draw(buffer: &mut [u8]) {
    // SAFETY: 缓冲区有效；该调用不会失败。
    unsafe { invoke(call::CPRNG_DRAW, &[buffer.as_mut_ptr() as usize, buffer.len()]) };
}

/// 向 CPRNG 混入熵（≤ 256 字节，超出部分分批）。
pub fn cprng_add_entropy(buffer: &[u8]) {
    for chunk in buffer.chunks(256) {
        // SAFETY: 缓冲区有效；超限已分批，混入失败没有可恢复动作。
        let _ = unsafe { invoke(call::CPRNG_ADD_ENTROPY, &[chunk.as_ptr() as usize, chunk.len()]) };
    }
}

/// 读取系统信息到 `buffer`，返回实际字节数。
///
/// # Errors
///
/// `ERR_BUFFER_TOO_SMALL`、`ERR_NOT_SUPPORTED`。
pub fn get_info(topic: u32, buffer: &mut [u8]) -> Result<usize, Status> {
    let mut actual = 0usize;
    // SAFETY: 缓冲区与输出位置有效。
    check(unsafe {
        invoke(
            call::SYSTEM_GET_INFO,
            &[topic as usize, buffer.as_mut_ptr() as usize, buffer.len(), (&raw mut actual) as usize],
        )
    })?;
    Ok(actual)
}

/// 系统概况。
pub fn basic() -> SystemBasic {
    let mut basic = SystemBasic::default();
    // 主题固定、缓冲区大小精确，失败只可能是内核不变量破坏。
    get_info(native_abi::system::BASIC, crate::bytes_of_mut(&mut basic)).expect("system basic info");
    basic
}

/// 物理页分配器每个 order 的空闲块数（order N 的块为 2^N 页），末尾的全零 order 已截去。
///
/// # Errors
///
/// 结果缓冲分配失败（`ERR_NO_MEMORY`）。
pub fn free_blocks() -> Result<alloc::vec::Vec<u64>, Status> {
    let mut counts = alloc::vec::Vec::new();
    counts.try_reserve_exact(64).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    counts.resize(64, 0u64);
    // SAFETY: u64 数组按字节视图传给内核写入。
    let bytes = unsafe { core::slice::from_raw_parts_mut(counts.as_mut_ptr().cast::<u8>(), counts.len() * 8) };
    let actual = get_info(native_abi::system::FREE_BLOCKS, bytes)?;
    counts.truncate(actual / 8);
    while counts.last() == Some(&0) {
        counts.pop();
    }
    Ok(counts)
}

/// 关机或重启。
///
/// # Errors
///
/// 不是根 resource 或操作非法。
pub fn power(resource: &Resource, action: u32) -> Result<(), Status> {
    // SAFETY: 无指针参数。
    check(unsafe { invoke(call::SYSTEM_POWER, &[resource.raw() as usize, action as usize]) })
}
