//! 调试串口输入：中断把收到的字节放进有界输入环，deferred work 唤醒阻塞读者。
//!
//! 环满时丢弃新到字节而不是覆盖未读字节：调试串口是交互输入，保留先到的字节使已读部分保持连续。

use core::sync::atomic::{AtomicBool, Ordering};

use crate::execution::sync::{IrqMutex, WaitQueue, WaitResult};

const CAPACITY: usize = 4096;

struct Ring {
    bytes: [u8; CAPACITY],
    head: usize,
    length: usize,
}

// OWNER: 输入字节的唯一缓冲；中断与任务上下文都会访问，必须 IRQ-safe。
static INPUT: IrqMutex<Ring> = IrqMutex::new(Ring {
    bytes: [0; CAPACITY],
    head: 0,
    length: 0,
});
// FLAG: 有新字节尚未通知读者；中断置位、deferred work 清除。缺失时读者可能错过只差一次的唤醒。
static PENDING: AtomicBool = AtomicBool::new(false);
// OWNER: 阻塞读取的读者。
static READERS: WaitQueue = WaitQueue::new();

/// 中断上下文：发布收到的字节。
pub(crate) fn publish_received(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    {
        let mut ring = INPUT.lock();
        for &byte in bytes {
            if ring.length == CAPACITY {
                break;
            }
            let tail = (ring.head + ring.length) % CAPACITY;
            ring.bytes[tail] = byte;
            ring.length += 1;
        }
    }
    PENDING.store(true, Ordering::Release);
}

/// trap 路径：是否需要发布 deferred 通知。
pub(crate) fn serial_input_pending() -> bool {
    PENDING.load(Ordering::Acquire)
}

/// deferred work：唤醒读者。
pub(crate) fn serial_input_ready() {
    if PENDING.swap(false, Ordering::AcqRel) {
        READERS.wake_all();
    }
}

/// 读取至多 `output.len()` 字节；没有输入时等待到 `deadline_ns`（`None` 表示不等待）。
///
/// # Errors
/// 等待超时、被打断或分配失败时返回对应 [`WaitResult`]。
pub(crate) fn serial_read(output: &mut [u8], deadline_ns: Option<u64>) -> Result<usize, WaitResult> {
    loop {
        {
            let mut ring = INPUT.lock();
            if ring.length != 0 || output.is_empty() {
                let count = ring.length.min(output.len());
                for slot in &mut output[..count] {
                    *slot = ring.bytes[ring.head];
                    ring.head = (ring.head + 1) % CAPACITY;
                    ring.length -= 1;
                }
                return Ok(count);
            }
        }
        let Some(deadline) = deadline_ns else {
            return Ok(0);
        };
        match READERS.wait_until(|| INPUT.lock().length != 0, Some(deadline)) {
            WaitResult::Woken => {}
            other => return Err(other),
        }
    }
}
