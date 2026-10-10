//! 实际 IPC registry 的有序只读投影；文本、PID/UID view 属于 proc codec。
use super::super::{IpcNamespace, MessageQueueSnapshot, SemaphoreSnapshot, SharedMemorySnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IpcTable {
    Message,
    Semaphore,
    SharedMemory,
}
impl IpcTable {
    pub(crate) const ALL: [Self; 3] = [Self::Message, Self::Semaphore, Self::SharedMemory];
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            Self::Message => b"msg",
            Self::Semaphore => b"sem",
            Self::SharedMemory => b"shm",
        }
    }
}
pub(crate) enum IpcDescription {
    Message(MessageQueueSnapshot),
    Semaphore(SemaphoreSnapshot),
    SharedMemory {
        snapshot: SharedMemorySnapshot,
        resident_pages: usize,
        swapped_pages: usize,
    },
}
impl IpcNamespace {
    /// cursor 是实际 registry slot（非 ID/Vec index）；每次仅 pin 一条 metadata，guard 不跨 formatter。
    /// 返回下一 slot 与真实行，末尾返回 None；全程无分配，也不复制 registry/页状态。
    pub(crate) fn next_description(
        &self,
        table: IpcTable,
        cursor: u32,
    ) -> Option<(u32, IpcDescription)> {
        if cursor >= super::super::ids::IPC_ID_SLOTS {
            return None;
        }
        let first = cursor as u16;
        match table {
            IpcTable::Message => {
                let registry = self.messages.lock();
                let (&slot, queue) = registry.ids.at_or_after(first)?;
                Some((
                    u32::from(slot) + 1,
                    IpcDescription::Message(queue.description()),
                ))
            }
            IpcTable::Semaphore => {
                let registry = self.semaphores.lock();
                let (&slot, set) = registry.ids.at_or_after(first)?;
                Some((
                    u32::from(slot) + 1,
                    IpcDescription::Semaphore(set.description()),
                ))
            }
            IpcTable::SharedMemory => {
                let registry = self.shared_memory.lock();
                let (&slot, segment) = registry.ids.at_or_after(first)?;
                let (snapshot, resident_pages, swapped_pages) = segment.description();
                Some((
                    u32::from(slot) + 1,
                    IpcDescription::SharedMemory {
                        snapshot,
                        resident_pages,
                        swapped_pages,
                    },
                ))
            }
        }
    }
}
