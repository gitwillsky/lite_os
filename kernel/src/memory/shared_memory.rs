//! System V 共享映射的实际 VMA membership；backing Arc 本身不表示挂接。
#[path = "shared_memory/statistics.rs"]
mod statistics;
pub(in crate::memory) use statistics::{SharedPageResidency, SharedPageStatistics};

use super::MemoryError;
use crate::sync::{TaskMutex, TaskMutexGuard};
use alloc::sync::Arc;
use core::fmt::Debug;
use core::sync::atomic::{AtomicBool, Ordering};

use super::SharedMemoryBacking;

/// 一个实际 anonymous shared backing 的 swap admission；与 IPC 元数据/配额分属不同事实。
/// PageOp→此 gate，SHM_LOCK 在 namespace/segment guard 外等待此 gate，随后短事务发布。
pub(in crate::memory) struct SharedMemorySwapPolicy {
    gate: TaskMutex<()>,
    // OWNER: 唯一 unevictable fact。Acquire/Release 只用于 stat 投影，mutation 必须同时持 gate。
    locked: AtomicBool,
}
impl Debug for SharedMemorySwapPolicy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedMemorySwapPolicy")
            .finish_non_exhaustive()
    }
}
pub(crate) struct SharedMemorySwapPolicyGuard<'a> {
    policy: &'a SharedMemorySwapPolicy,
    _gate: TaskMutexGuard<'a, ()>,
}
impl SharedMemorySwapPolicy {
    pub(in crate::memory) fn new() -> Self {
        Self {
            gate: TaskMutex::new(()),
            locked: AtomicBool::new(false),
        }
    }
    pub(in crate::memory) fn locked(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }
    pub(in crate::memory) fn freeze(&self) -> Result<SharedMemorySwapPolicyGuard<'_>, MemoryError> {
        Ok(SharedMemorySwapPolicyGuard {
            policy: self,
            _gate: self.gate.lock().map_err(|_| MemoryError::OutOfMemory)?,
        })
    }
    pub(in crate::memory) fn try_swap(&self) -> Option<TaskMutexGuard<'_, ()>> {
        let guard = self.gate.try_lock()?;
        (!self.locked()).then_some(guard)
    }
}
impl SharedMemorySwapPolicyGuard<'_> {
    /// caller 在实际 segment guard 内同时移交配额 capability；本 guard 封闭在途 swap。
    pub(crate) fn set_locked(&self, value: bool) {
        self.policy.locked.store(value, Ordering::Release);
    }
}

/// task 装配的标准 shm VMA 生命周期；每次调用取得实际执行者的 PID 与 realtime。
/// 实现不得持有 Task/mm；open 不分配，close 的最终 backing 退休须在 MM guard 外。
pub(crate) trait SharedMemoryVma: Debug + Send + Sync {
    /// 一个实际 VMA 新增（初始映射、fork 或 split）。
    fn open(&self);
    /// 一个实际 VMA 在 translation fence 后释放。
    fn close(&self);
}

/// 已通过 IPC DAC 和临时 nattch reservation 的映射输入；clone 只保活准备资源。
#[derive(Clone, Debug)]
pub(crate) struct SharedMemoryMappingSource {
    pub(crate) backing: Arc<SharedMemoryBacking>,
    pub(crate) lifetime: Arc<dyn SharedMemoryVma>,
    /// SHM_RDONLY 的 VM_MAYWRITE ceiling 不随 IPC_SET 或 mprotect 改变。
    pub(crate) may_write: bool,
}

/// 每个实际 MapArea 恰好拥有一个计数；临时 source/observer Arc 不进入 nattch。
#[derive(Debug)]
pub(in crate::memory) struct SharedMemoryVmaMembership {
    pub(in crate::memory) lifetime: Arc<dyn SharedMemoryVma>,
    pub(in crate::memory) may_write: bool,
}
impl SharedMemoryVmaMembership {
    pub(in crate::memory) fn new(source: SharedMemoryMappingSource) -> Self {
        source.lifetime.open();
        Self {
            lifetime: source.lifetime,
            may_write: source.may_write,
        }
    }
}
impl Clone for SharedMemoryVmaMembership {
    fn clone(&self) -> Self {
        self.lifetime.open();
        Self {
            lifetime: self.lifetime.clone(),
            may_write: self.may_write,
        }
    }
}
impl Drop for SharedMemoryVmaMembership {
    fn drop(&mut self) {
        self.lifetime.close();
    }
}
