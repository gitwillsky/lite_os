//! Port：异步事件队列。
//!
//! 队列容量不变量：`capacity >= len + reserved`。`object_wait_async` 登记时预留一个空位，
//! 因此在其他对象的 signal 临界区内投递 packet 永远不需要分配。

use alloc::collections::VecDeque;

use native_abi::{PortPacket, Status, signals};
use spin::Mutex;

use super::signals::{CancelKey, SignalState, WaitOutcome};
use crate::execution::sync::lock_with_spare;

struct PortQueue {
    packets: VecDeque<PortPacket>,
    // OWNER: 已登记但尚未投递的异步等待数；缺失时投递可能在持其他对象锁时触发扩容分配。
    reserved: usize,
}

/// 异步事件端口。
pub(crate) struct Port {
    koid: u64,
    queue: Mutex<PortQueue>,
    // OWNER: 只承载内部 READABLE，供 port_wait 复用唯一的 observer 等待机制；不对外开放 wait_async。
    readable: SignalState,
}

impl Port {
    /// 构造空 port。
    pub(crate) fn new() -> Self {
        Self {
            koid: super::next_koid(),
            queue: Mutex::new(PortQueue {
                packets: VecDeque::new(),
                reserved: 0,
            }),
            readable: SignalState::new(0),
        }
    }

    /// 为一次异步等待预留队列空位。
    pub(super) fn reserve(&self) -> Result<(), Status> {
        let mut queue = lock_with_spare(
            &self.queue,
            |queue| queue.reserved + 1,
            |queue| &mut queue.packets,
        )
        .map_err(|()| native_abi::ERR_NO_MEMORY)?;
        queue.reserved += 1;
        Ok(())
    }

    /// 归还未使用的预留。
    pub(super) fn release_reservation(&self) {
        let mut queue = self.queue.lock();
        queue.reserved = queue
            .reserved
            .checked_sub(1)
            .expect("port reservation released twice");
    }

    /// 消费一个预留并入队；不分配。
    pub(super) fn deliver(&self, packet: PortPacket) {
        let mut queue = self.queue.lock();
        queue.reserved = queue
            .reserved
            .checked_sub(1)
            .expect("port packet delivered without reservation");
        debug_assert!(queue.packets.len() < queue.packets.capacity());
        queue.packets.push_back(packet);
        // READABLE 与队列在同一临界区变化（锁序 queue → readable）；分开发布会在并发取空后留下
        // 没有 packet 的 READABLE，使等待者空转。
        self.readable.update(0, signals::READABLE);
    }

    /// 投递一个 packet（用户 packet、pager 请求、中断通知）。
    ///
    /// # Errors
    /// 队列扩容失败返回 `ERR_NO_MEMORY`。
    pub(crate) fn queue(&self, packet: PortPacket) -> Result<(), Status> {
        self.reserve()?;
        self.deliver(packet);
        Ok(())
    }

    /// 取出队首 packet，必要时等待。
    ///
    /// # Parameters
    /// - `cancel`: 调用所经句柄，句柄关闭会取消等待。
    /// - `deadline_ns`: 单调时钟绝对截止时间。
    ///
    /// # Errors
    /// `ERR_TIMED_OUT`、`ERR_CANCELED`、`ERR_NO_MEMORY`；进程终止返回 `ERR_CANCELED`。
    pub(crate) fn wait(
        &self,
        cancel: CancelKey,
        deadline_ns: Option<u64>,
    ) -> Result<PortPacket, Status> {
        loop {
            {
                let mut queue = self.queue.lock();
                if let Some(packet) = queue.packets.pop_front() {
                    if queue.packets.is_empty() {
                        // 同一临界区清除，避免与并发投递交错后丢失 READABLE。
                        self.readable.update(signals::READABLE, 0);
                    }
                    return Ok(packet);
                }
            }
            match self.readable.wait(signals::READABLE, cancel, deadline_ns)? {
                WaitOutcome::Satisfied(_) => {}
                WaitOutcome::TimedOut(_) => return Err(native_abi::ERR_TIMED_OUT),
                WaitOutcome::Canceled | WaitOutcome::Interrupted => {
                    return Err(native_abi::ERR_CANCELED);
                }
            }
        }
    }
}

impl super::KernelObject for Port {
    fn object_type(&self) -> native_abi::ObjectType {
        native_abi::ObjectType::Port
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        super::WAITABLE_RIGHTS | native_abi::rights::READ | native_abi::rights::WRITE
    }
    /// port 不可被 wait_async 观察（避免 port 之间的投递环）；只取消其自身 port_wait 的等待。
    fn cancel(&self, key: CancelKey) {
        self.readable.cancel(key);
    }
}
