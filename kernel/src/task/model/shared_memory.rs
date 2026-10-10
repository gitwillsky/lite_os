//! 将实际执行者 identity/clock 装配到 memory 的标准 shm VMA 生命周期。
use super::TaskControlBlock;
use crate::{
    ipc::{IpcNamespace, SharedMemoryAttachReservation, SharedMemorySegment},
    memory::{MapPermission, MemoryError, SharedMemoryMappingSource, SharedMemoryVma},
};
use alloc::sync::Arc;

/// 一次 shmat 对应 Linux vm_file identity；所有 fork/split fragments 共享此身份。
/// 不持有 Task、MM 或 PID membership；PID 在每次实际 open/close 时取得。
#[derive(Debug)]
struct Attachment {
    namespace: Arc<IpcNamespace>,
    segment: Arc<SharedMemorySegment>,
}
impl SharedMemoryVma for Attachment {
    fn open(&self) {
        self.namespace.shared_memory_open(
            &self.segment,
            crate::task::current_tgid_identity(),
            crate::timer::get_realtime_ns() / 1_000_000_000,
        );
    }
    fn close(&self) {
        self.namespace.shared_memory_close(
            &self.segment,
            Some((
                crate::task::current_tgid_identity(),
                crate::timer::get_realtime_ns() / 1_000_000_000,
            )),
        );
    }
}
impl TaskControlBlock {
    /// 权限和收费 real UID 来自同一凭据，实际 RLIMIT_MEMLOCK 来自本进程限额 owner。
    pub(crate) fn lock_shared_memory(
        &self,
        id: i32,
        locking: bool,
    ) -> Result<(), crate::ipc::IpcError> {
        let (access, real_uid) = self.ipc_access_with_real_uid();
        self.ipc_namespace()
            .shared_memory_lock(id, locking, &access, real_uid, || {
                self.resource_limit(super::RLIMIT_MEMLOCK)
                    .expect("RLIMIT_MEMLOCK exists")
                    .soft
            })
    }
    /// shmget 的创建登记属于 calling Thread；首次节点 owner 在 Thread guard 外准备。
    pub(crate) fn create_shared_memory(
        &self,
        key: i32,
        size: usize,
        flags: u32,
    ) -> Result<i32, crate::ipc::IpcError> {
        let existing = self.thread().shm_creator.lock().clone();
        let creator = if let Some(existing) = existing {
            existing
        } else {
            let prepared = crate::ipc::SharedMemoryCreatorList::new()?;
            let mut slot = self.thread().shm_creator.lock();
            let selected = slot.get_or_insert_with(|| prepared.clone()).clone();
            drop(slot);
            drop(prepared);
            selected
        };
        self.ipc_namespace().shared_memory_get(
            key,
            size,
            flags,
            &self.ipc_access(),
            self.tgid_identity().clone(),
            crate::timer::get_realtime_ns() / 1_000_000_000,
            &creator,
        )
    }
    pub(in crate::task) fn retire_shared_memory_creator(&self) {
        let creator = self.thread().shm_creator.lock().take();
        if let Some(creator) = creator {
            creator.retire();
        }
    }
    /// 已授权 reservation 一直跨越 MM mutation；失败也在 MM guard 外归还临时 nattch。
    pub(crate) fn attach_shared_memory(
        &self,
        reservation: SharedMemoryAttachReservation,
        address: usize,
        permission: MapPermission,
        may_write: bool,
        remap: bool,
    ) -> Result<usize, MemoryError> {
        let lifetime = Arc::try_new(Attachment {
            namespace: reservation.namespace.clone(),
            segment: reservation.segment.clone(),
        })
        .map_err(|_| MemoryError::OutOfMemory)?;
        let source = SharedMemoryMappingSource {
            backing: reservation.segment.backing(),
            lifetime,
            may_write,
        };
        let result = self.install_user_mapping(crate::memory::UserMappingRequest {
            address,
            length: reservation.segment.size,
            permission,
            placement: if remap {
                crate::memory::MappingPlacement::Fixed
            } else if address != 0 {
                crate::memory::MappingPlacement::FixedNoReplace
            } else {
                crate::memory::MappingPlacement::Hint
            },
            source: crate::memory::UserMappingSource::SystemV(source),
        });
        drop(reservation);
        result
    }
    /// 原始 shmat 起点选中同一次挂接的 fragments；先摘除，guard 外 fence/实际 close。
    pub(crate) fn detach_shared_memory(&self, address: usize) -> Result<(), MemoryError> {
        let mm = self.address_space();
        let retirement = {
            let mut memory = mm.memory_set.lock().map_err(|_| MemoryError::OutOfMemory)?;
            memory.prepare_shared_memory_detach(address)?
        };
        retirement.synchronize();
        Ok(())
    }
}
