//! SysV 共享内存：段的后备对象，以及映射对段附加计数（`shm_nattch`）的成员关系。

use alloc::sync::Arc;
use core::fmt::Debug;
use core::sync::atomic::{AtomicBool, Ordering};

use super::{MemoryError, MemoryObject, PAGE_SIZE};
use crate::sync::{TaskMutex, TaskMutexGuard};

/// 一个段的内容：匿名 VMO 与 `SHM_LOCK` 状态。
#[derive(Debug)]
pub(crate) struct SharedMemoryBacking {
    object: Arc<MemoryObject>,
    // OWNER: 串行化 SHM_LOCK/SHM_UNLOCK 与配额收费；`locked` 只在持有它时修改。
    lock_gate: TaskMutex<()>,
    locked: AtomicBool,
}

/// 持有 `SHM_LOCK` 修改权的凭证。
pub(crate) struct SharedMemoryLockGuard<'a> {
    backing: &'a SharedMemoryBacking,
    _gate: TaskMutexGuard<'a, ()>,
}

impl SharedMemoryLockGuard<'_> {
    /// 记录锁定状态（内核不换出页，锁定只影响 `SHM_LOCKED` 与配额）。
    pub(crate) fn set_locked(&self, value: bool) {
        self.backing.locked.store(value, Ordering::Release);
    }
}

impl SharedMemoryBacking {
    /// 分配 `pages` 页零内容段。
    ///
    /// # Errors
    ///
    /// 内存不足。
    pub(crate) fn allocate(pages: usize) -> Result<Arc<Self>, MemoryError> {
        let bytes = pages
            .checked_mul(PAGE_SIZE)
            .ok_or(MemoryError::OutOfMemory)?;
        Arc::try_new(Self {
            object: MemoryObject::anonymous(bytes, false)?,
            lock_gate: TaskMutex::new(()),
            locked: AtomicBool::new(false),
        })
        .map_err(|_| MemoryError::OutOfMemory)
    }

    /// 后备对象。
    pub(crate) fn object(&self) -> &Arc<MemoryObject> {
        &self.object
    }

    /// `SHM_LOCKED`。
    pub(crate) fn locked(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }

    /// 取得锁定状态的修改权。
    ///
    /// # Errors
    ///
    /// 等待者分配失败。
    pub(crate) fn freeze_lock_state(&self) -> Result<SharedMemoryLockGuard<'_>, MemoryError> {
        Ok(SharedMemoryLockGuard {
            backing: self,
            _gate: self
                .lock_gate
                .lock()
                .map_err(|_| MemoryError::OutOfMemory)?,
        })
    }

    /// `(驻留页, 换出页)`；内核不换出页，后者恒为 0。
    pub(crate) fn shared_memory_statistics(&self) -> (usize, usize) {
        (
            self.object.committed_bytes().unwrap_or(0) as usize / PAGE_SIZE,
            0,
        )
    }
}

/// 一次 `shmat` 的附加身份：每个实际 VMA 新增时 `open`，释放时 `close`（维护 nattch、atime/dtime）。
pub(crate) trait SharedMemoryVma: Debug + Send + Sync {
    fn open(&self);
    fn close(&self);
}

/// 已授权的映射输入。
#[derive(Clone, Debug)]
pub(crate) struct SharedMemoryMappingSource {
    pub(crate) backing: Arc<SharedMemoryBacking>,
    pub(crate) lifetime: Arc<dyn SharedMemoryVma>,
    /// `SHM_RDONLY` 决定的写权限上限。
    pub(crate) may_write: bool,
}

/// 一个 VMA 对段的成员关系：克隆（拆分、fork）时 `open`，释放时 `close`。
#[derive(Debug)]
pub(crate) struct SharedMemoryMembership {
    pub(crate) source: SharedMemoryMappingSource,
}

impl SharedMemoryMembership {
    pub(crate) fn new(source: SharedMemoryMappingSource) -> Self {
        source.lifetime.open();
        Self { source }
    }
}

impl Clone for SharedMemoryMembership {
    fn clone(&self) -> Self {
        self.source.lifetime.open();
        Self {
            source: self.source.clone(),
        }
    }
}

impl Drop for SharedMemoryMembership {
    fn drop(&mut self) {
        self.source.lifetime.close();
    }
}
