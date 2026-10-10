//! 每个实际 socket 的 ancillary allocation ledger；预收费 token 覆盖准备、user-copy、发送和清理。
use super::{NetNamespace, SocketError};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Socket 唯一网络 association 与 option allocation budget；dup/SCM_RIGHTS 不复制它。
pub(crate) struct SocketMemory {
    namespace: Arc<NetNamespace>,
    // OWNER: 在途 option allocation 原始申请字节的唯一和；不含36-byte stack control。
    // 只对 scalar 收费，不发表 payload，Relaxed CAS 原子化 admission，不借 Task/FD/OFD。
    used: AtomicUsize,
}
impl SocketMemory {
    pub(crate) fn try_new(namespace: Arc<NetNamespace>) -> Result<Arc<Self>, SocketError> {
        Arc::try_new(Self {
            namespace,
            used: AtomicUsize::new(0),
        })
        .map_err(|_| SocketError::NoMemory)
    }
    pub(crate) fn namespace(&self) -> Arc<NetNamespace> {
        self.namespace.clone()
    }
    pub(in crate::socket) fn namespace_ref(&self) -> &NetNamespace {
        &self.namespace
    }
    /// Linux sock_kmalloc：size <= max 且 used+size < max，收费发生在可能睡眠的 allocation 前。
    pub(crate) fn reserve(self: &Arc<Self>, bytes: usize) -> Option<ControlMemoryCharge> {
        let maximum = self.namespace.optmem_max();
        if bytes > maximum {
            return None;
        }
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let next = used.checked_add(bytes)?;
            if next >= maximum {
                return None;
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => {
                    return Some(ControlMemoryCharge {
                        memory: self.clone(),
                        bytes,
                    });
                }
                Err(current) => used = current,
            }
        }
    }
}
/// 独占 allocation 费用；只 pin 实际 memory owner，不保活 Task 或 socket backend。
pub(crate) struct ControlMemoryCharge {
    memory: Arc<SocketMemory>,
    bytes: usize,
}
impl Drop for ControlMemoryCharge {
    fn drop(&mut self) {
        let previous = self.memory.used.fetch_sub(self.bytes, Ordering::Relaxed);
        assert!(previous >= self.bytes, "duplicate socket optmem refund");
    }
}
