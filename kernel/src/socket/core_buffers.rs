//! Linux 全局 socket buffer tunables；非 initial NET namespace 也读取同一 owner，不能复制。
use super::{CoreBufferLimit, SocketIoDirection};
use core::sync::atomic::{AtomicU32, Ordering};
// OWNER: 固定 Linux sysctl_{w,r}mem_{default,max} 唯一可变值。各 scalar 的 Relaxed load/store
// 对应 READ_ONCE/WRITE_ONCE，没有跨字段 transaction、发布其他对象或排队信息；不得持 Task/MM。
static LIMITS: [AtomicU32; 4] = [
    AtomicU32::new(212_992),
    AtomicU32::new(212_992),
    AtomicU32::new(4 << 20),
    AtomicU32::new(4 << 20),
];
impl CoreBufferLimit {
    pub(crate) fn value(self) -> u32 {
        LIMITS[self.index()].load(Ordering::Relaxed)
    }
    /// 原生 2048 + actual packet metadata 的 buffer 下限；同一公式用于 syscall policy。
    pub(crate) fn minimum(self) -> u32 {
        let direction = match self {
            Self::SendDefault | Self::SendMaximum => SocketIoDirection::Send,
            _ => SocketIoDirection::Receive,
        };
        super::unix::minimum_buffer(direction) as u32
    }
    /// 调优只改变未来 default capture/option clamp，不改已有 socket 的真实 ledger。
    pub(crate) fn set(self, value: u32) -> Result<(), crate::namespace::NamespaceError> {
        if value < self.minimum() || value > i32::MAX as u32 {
            return Err(crate::namespace::NamespaceError::Invalid);
        }
        LIMITS[self.index()].store(value, Ordering::Relaxed);
        Ok(())
    }
}
