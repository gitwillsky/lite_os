//! 按地址等待与唤醒。

use core::sync::atomic::AtomicU32;

use native_abi::{Status, call};

use crate::syscall::{check, invoke};

/// 若 `*word == expected` 则阻塞到被唤醒或 `deadline`。
///
/// # Errors
///
/// `ERR_BAD_STATE`（值已改变）、`ERR_TIMED_OUT`、`ERR_CANCELED`（线程被终止）。
pub fn wait(word: &AtomicU32, expected: u32, deadline: i64, options: u32) -> Result<(), Status> {
    // SAFETY: `word` 是有效、4 字节对齐的地址。
    check(unsafe {
        invoke(
            call::FUTEX_WAIT,
            &[
                word.as_ptr() as usize,
                expected as usize,
                deadline as usize,
                options as usize,
            ],
        )
    })
}

/// 唤醒至多 `count` 个等待者，返回实际唤醒数。
pub fn wake(word: &AtomicU32, count: u32, options: u32) -> u32 {
    let mut woken = 0u32;
    // SAFETY: 地址与输出位置有效；对有效地址的唤醒不会失败。
    unsafe {
        invoke(
            call::FUTEX_WAKE,
            &[
                word.as_ptr() as usize,
                count as usize,
                options as usize,
                (&raw mut woken) as usize,
            ],
        )
    };
    woken
}

/// 以原始地址等待（目标地址属于调用者地址空间中其他代码管理的内存）。
///
/// # Errors
///
/// 同 [`wait`]，另有 `ERR_INVALID_POINTER`。
pub fn wait_address(
    address: usize,
    expected: u32,
    deadline: i64,
    options: u32,
) -> Result<(), Status> {
    // SAFETY: 内核校验地址可访问；不访问调用者其余内存。
    check(unsafe {
        invoke(
            call::FUTEX_WAIT,
            &[
                address,
                expected as usize,
                deadline as usize,
                options as usize,
            ],
        )
    })
}

/// 以原始地址唤醒，返回实际唤醒数。
///
/// # Errors
///
/// `ERR_INVALID_POINTER`、`ERR_INVALID_ARGS`。
pub fn wake_address(address: usize, count: u32, options: u32) -> Result<u32, Status> {
    let mut woken = 0u32;
    // SAFETY: 输出位置有效；内核校验目标地址。
    check(unsafe {
        invoke(
            call::FUTEX_WAKE,
            &[
                address,
                count as usize,
                options as usize,
                (&raw mut woken) as usize,
            ],
        )
    })?;
    Ok(woken)
}

/// 唤醒 `wake_count` 个并把至多 `requeue_count` 个移到 `target`（原始地址）。
///
/// # Errors
///
/// `ERR_BAD_STATE`（`*address != expected`）、`ERR_INVALID_POINTER`。
pub fn requeue_address(
    address: usize,
    wake_count: u32,
    expected: u32,
    target: usize,
    requeue_count: u32,
    options: u32,
) -> Result<(), Status> {
    // SAFETY: 内核校验两个地址。
    check(unsafe {
        invoke(
            call::FUTEX_REQUEUE,
            &[
                address,
                wake_count as usize,
                expected as usize,
                target,
                requeue_count as usize,
                options as usize,
            ],
        )
    })
}
