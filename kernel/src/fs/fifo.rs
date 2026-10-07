//! 命名管道（FIFO）：路径上的 inode 与内核 [`Pipe`] 的绑定，以及 Linux `fifo_open` 的 open 汇合语义。
//!
//! FIFO inode 本身不保存数据；第一个 open 创建 Pipe，最后一个 endpoint 关闭时 Pipe 随之消失（未读数据
//! 丢弃）。同一 inode 的所有 open 通过 `(filesystem, inode)` 找到同一个 Pipe。

use alloc::{sync::Arc, sync::Weak, vec::Vec};
use spin::Mutex;

use crate::{
    ipc::{Pipe, PipeDirection, PipeEnd},
    sync::WaitResult,
};

/// FIFO open 的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FifoOpenError {
    /// 非阻塞只写打开时没有 reader（`ENXIO`）。
    NoReader,
    /// 等待对端期间被 signal 中断（`EINTR`/重启）。
    Interrupted,
    /// 分配失败（`ENOMEM`）。
    OutOfMemory,
}

struct Slot {
    identity: (usize, u64),
    pipe: Weak<Pipe>,
}

// OWNER: FIFO inode identity → 内核 Pipe 的唯一映射；只持 `Weak`，Pipe 的生命周期由 endpoint 决定。
// 缺失时同一 FIFO 的两次 open 会各自得到独立 Pipe，reader 与 writer 永远看不到彼此。
static FIFOS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

/// 取得（必要时创建）`identity` 对应的 Pipe。
fn pipe_for(identity: (usize, u64)) -> Result<Arc<Pipe>, FifoOpenError> {
    let mut slots = FIFOS.lock();
    if let Some(slot) = slots.iter().find(|slot| slot.identity == identity)
        && let Some(pipe) = slot.pipe.upgrade()
    {
        return Ok(pipe);
    }
    let pipe = Pipe::new_named().map_err(|()| FifoOpenError::OutOfMemory)?;
    // 先回收已死槽位，再复用或追加：死槽位只是 Weak，不会无限增长。
    slots.retain(|slot| slot.pipe.strong_count() != 0 && slot.identity != identity);
    slots
        .try_reserve(1)
        .map_err(|_| FifoOpenError::OutOfMemory)?;
    slots.push(Slot {
        identity,
        pipe: Arc::downgrade(&pipe),
    });
    Ok(pipe)
}

/// 打开 FIFO 的一端（Linux `fifo_open`）。
///
/// 1. 只读：登记 reader；没有 writer 时，非阻塞立即成功，阻塞则等到自本次 open 起有 writer 打开过；
/// 2. 只写：没有 reader 时，非阻塞返回 [`FifoOpenError::NoReader`]；阻塞则登记 writer 后等 reader。
///
/// 等待“自开始等待起对端又打开过”而不是“此刻对端存在”：对端打开后立刻关闭也必须放行等待者。
///
/// # Parameters
///
/// - `identity`: FIFO inode 的 `(filesystem_id, inode number)`。
/// - `direction`: 本次 open 的方向（`O_RDWR` 不支持，由调用者拒绝）。
/// - `nonblocking`: `O_NONBLOCK`。
///
/// # Errors
///
/// 见 [`FifoOpenError`]；等待被中断时已登记的 endpoint 随返回一并撤销。
pub(crate) fn open(
    identity: (usize, u64),
    direction: PipeDirection,
    nonblocking: bool,
) -> Result<Arc<PipeEnd>, FifoOpenError> {
    let pipe = pipe_for(identity)?;
    let (peers, opened_before) = pipe.peer_snapshot(direction);
    if direction == PipeDirection::Write && peers == 0 && nonblocking {
        return Err(FifoOpenError::NoReader);
    }
    let end = pipe
        .open_end(direction)
        .map_err(|()| FifoOpenError::OutOfMemory)?;
    if peers == 0 && !nonblocking {
        match pipe.wait_for_peer(direction, opened_before) {
            WaitResult::Woken | WaitResult::TimedOut => {}
            WaitResult::Interrupted => return Err(FifoOpenError::Interrupted),
            WaitResult::OutOfMemory => return Err(FifoOpenError::OutOfMemory),
        }
    }
    Ok(end)
}
