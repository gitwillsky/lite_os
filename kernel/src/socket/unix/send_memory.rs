//! AF_UNIX 实际 sender memory ledger；原生 admission watermark 与 POLLOUT quarter-watermark 分别投影同一 used。
use super::UnixSocket;
use crate::socket::{CoreBufferLimit, SocketError, SocketIoDirection};
use alloc::sync::Arc;

/// 一个 endpoint 的实际 send limit 与在途 memory，不保存第二份队列长度或 readiness。
pub(super) struct SendMemoryState {
    send_limit: usize,
    receive_limit: usize,
    used: usize,
}
impl SendMemoryState {
    pub(super) fn new() -> Self {
        Self {
            send_limit: CoreBufferLimit::SendDefault.value() as usize,
            receive_limit: CoreBufferLimit::ReceiveDefault.value() as usize,
            used: 0,
        }
    }
}

/// 单条准备中或入队 packet 的实际 sender/费用；source observer 和退款 capability 是同一 owner。
pub(super) struct SenderCharge {
    sender: Arc<UnixSocket>,
    bytes: usize,
}
impl SenderCharge {
    pub(super) fn sender(&self) -> &Arc<UnixSocket> {
        &self.sender
    }
    /// exact reserve 的实际 capacity 若大于申请量，计入真实追加 storage；不重新做第二次 admission。
    pub(super) fn account_capacity(&mut self, requested: usize, actual: usize) {
        let extra = actual
            .checked_sub(requested)
            .expect("packet allocator capacity below request");
        if extra != 0 {
            let mut buffers = self.sender.send_memory.lock();
            buffers.used = buffers
                .used
                .checked_add(extra)
                .expect("sender memory overflow");
            self.bytes += extra;
        }
    }
}
impl Drop for SenderCharge {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        let mut buffers = self.sender.send_memory.lock();
        let previous = buffers.used;
        buffers.used = previous
            .checked_sub(self.bytes)
            .expect("duplicate sender memory refund");
        // 只有 allocation 或 quarter-watermark 从不可用变为可用才发布 edge；普通小额退款不惊扰全部 waiter。
        let quarter = buffers.send_limit / 4;
        let notify = (previous >= buffers.send_limit && buffers.used < buffers.send_limit)
            || (previous.saturating_add(1) > quarter && buffers.used.saturating_add(1) <= quarter);
        drop(buffers);
        if notify {
            self.sender.notify();
        }
    }
}
impl UnixSocket {
    /// 按 Linux unsigned clamp、倍增与 actual metadata floor 更新唯一 buffer policy。
    /// RCVBUF 是 AF_UNIX native hint，不另加 receiver bytes 上限；send limit 直接用于同一 ledger。
    pub(crate) fn set_buffer_limit(&self, direction: SocketIoDirection, value: i32, forced: bool) {
        let maximum = match direction {
            SocketIoDirection::Send => CoreBufferLimit::SendMaximum,
            SocketIoDirection::Receive => CoreBufferLimit::ReceiveMaximum,
        };
        // SO_*BUFFORCE 在 syscall 已按 initial UserNS 的全局能力授权；负值原生归零，绕过全局 max。
        let requested = if forced {
            value.max(0) as usize
        } else {
            (value as u32 as usize).min(maximum.value() as usize)
        }
        .min(i32::MAX as usize / 2);
        let mut buffers = self.send_memory.lock();
        let floor = super::minimum_buffer(direction);
        match direction {
            SocketIoDirection::Send => buffers.send_limit = (requested * 2).max(floor),
            SocketIoDirection::Receive => buffers.receive_limit = (requested * 2).max(floor),
        }
        drop(buffers);
        self.notify();
    }
    /// actual option value；getter 不重新计算或缓存默认/当前 policy。
    pub(crate) fn buffer_limit(&self, direction: SocketIoDirection) -> i32 {
        let buffers = self.send_memory.lock();
        match direction {
            SocketIoDirection::Send => buffers.send_limit as i32,
            SocketIoDirection::Receive => buffers.receive_limit as i32,
        }
    }
    /// 原生 packet 长度限制来自真实 sndbuf，而非 IPv4 的 u16 payload 上限。
    pub(crate) fn validate_packet_length(&self, length: usize) -> Result<(), SocketError> {
        if length > self.send_memory.lock().send_limit.saturating_sub(32) {
            Err(SocketError::MessageTooLarge)
        } else {
            Ok(())
        }
    }
    /// 在 payload/tree allocation 前预留实际 memory；允许原生单次跨越 watermark，不放行下笔。
    pub(super) fn reserve_packet(
        self: &Arc<Self>,
        length: usize,
        overhead: usize,
    ) -> Result<Option<SenderCharge>, SocketError> {
        let mut buffers = self.send_memory.lock();
        if length > buffers.send_limit.saturating_sub(32) {
            return Err(SocketError::MessageTooLarge);
        }
        if buffers.used >= buffers.send_limit {
            return Ok(None);
        }
        let bytes = length.checked_add(overhead).ok_or(SocketError::NoMemory)?;
        buffers.used = buffers
            .used
            .checked_add(bytes)
            .ok_or(SocketError::NoMemory)?;
        Ok(Some(SenderCharge {
            sender: self.clone(),
            bytes,
        }))
    }

    /// 原生 stream 每段最多 sndbuf/2-64，credit 预留也覆盖潜在 frame node，最终只保留实际发布量。
    pub(super) fn reserve_stream(
        self: &Arc<Self>,
        requested: usize,
        overhead: usize,
    ) -> Result<Option<(SenderCharge, usize)>, SocketError> {
        let mut buffers = self.send_memory.lock();
        if buffers.used >= buffers.send_limit {
            return Ok(None);
        }
        let capacity = requested.min((buffers.send_limit / 2).saturating_sub(64));
        let bytes = capacity
            .checked_add(overhead)
            .ok_or(SocketError::NoMemory)?;
        buffers.used = buffers
            .used
            .checked_add(bytes)
            .ok_or(SocketError::NoMemory)?;
        Ok(Some((
            SenderCharge {
                sender: self.clone(),
                bytes,
            },
            capacity,
        )))
    }
    pub(super) fn memory_writable(&self) -> bool {
        let buffers = self.send_memory.lock();
        buffers.used.saturating_add(1) <= buffers.send_limit / 4
    }
    /// 同一 ledger 的 allocation watermark；terminal/half-close 由实际 capacity owner 重查。
    pub(super) fn send_memory_available(&self) -> bool {
        let buffers = self.send_memory.lock();
        buffers.used < buffers.send_limit
    }
}
