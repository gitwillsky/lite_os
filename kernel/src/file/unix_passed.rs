//! OFD 向 AF_UNIX rights graph 投影实际 file identity 与可锁外退休的 root probe pin。
use super::{OpenFileDescription, OpenFileKind};
use crate::net::{UnixNode, UnixPassedFile};
use alloc::sync::Arc;

impl UnixPassedFile for OpenFileDescription {
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn unix_node(&self) -> Option<UnixNode> {
        match &self.kind {
            OpenFileKind::Socket(socket) => socket.unix_node(),
            _ => None,
        }
    }

    fn probe_external(
        self: Arc<Self>,
        inflight: usize,
    ) -> (bool, Arc<dyn crate::net::UnixPassedFile>) {
        // 1. graph 每条 outgoing edge 持有一个 OFD Arc。
        // 2. Weak::upgrade 为本次 probe 临时增加一个 Arc。
        // 3. 超过两者的引用才是 descriptor/active syscall 等外部 root；漏掉 +1 会误保活 cycle。
        let rooted = Arc::strong_count(&self) > inflight.saturating_add(1);
        (rooted, self)
    }
}
