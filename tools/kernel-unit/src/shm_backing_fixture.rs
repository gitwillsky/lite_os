//! IPC registry 用例的页 owner 输入 seam；不执行页 fault、swap 或 VMA 算法。
//! 实际共享页/VMA 行为由 fork_file_eof 装配生产 memory 源码验证。
use alloc::sync::Arc;
#[path = "../../../kernel/src/memory/shared_memory.rs"]
#[allow(
    unused_imports,
    reason = "本 fixture 使用实际 shared VMA owner；页 residency 账本由 swap_page 回归覆盖"
)]
mod shared_memory;
#[derive(Debug)]
pub(crate) enum MemoryError {
    OutOfMemory,
}
pub(crate) const PAGE_SIZE: usize = 4096;
#[derive(Debug)]
pub(crate) struct SharedMemoryBacking {
    pub(crate) pages: usize,
    policy: shared_memory::SharedMemorySwapPolicy,
}
impl SharedMemoryBacking {
    pub(crate) fn locked(&self) -> bool {
        self.policy.locked()
    }
    pub(crate) fn freeze_swap_policy(
        &self,
    ) -> Result<shared_memory::SharedMemorySwapPolicyGuard<'_>, MemoryError> {
        self.policy.freeze()
    }
    pub(crate) fn shared_memory_statistics(&self) -> (usize, usize) {
        (0, 0)
    }
    pub(crate) fn allocate(pages: usize) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            pages,
            policy: shared_memory::SharedMemorySwapPolicy::new(),
        })
        .map_err(|_| ())
    }
}
