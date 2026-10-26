use super::*;
use crate::memory::{ReclaimRequest, ReclaimResult};
use core::sync::atomic::AtomicBool;

mod dumpability;
use dumpability::Dumpability;
mod mapping;
mod task_access;

/// Process owner 向 task façade 发布的 procfs 统计快照。
pub(in crate::task) struct ProcessStatistics {
    /// 原 leader 的 comm；exec 后为 survivor 名称，其他线程重命名不影响。
    pub(in crate::task) comm: Vec<u8>,
    /// Process 创建时的 monotonic 微秒时间。
    pub(in crate::task) start_time_us: u64,
    /// 原 leader 是否仍持 mm；page fields 为一次 group common projection，可供 active Thread 使用。
    pub(in crate::task) has_mm: bool,
    /// AddressSpace 用户 VMA 总页数。
    pub(in crate::task) virtual_pages: usize,
    /// AddressSpace 当前驻留页数。
    pub(in crate::task) resident_pages: usize,
    /// Private anonymous logical swap entries; shmem is not included in Linux VmSwap.
    pub(in crate::task) swapped_pages: usize,
    /// 当前驻留且由共享 mapping owner 持有的页数。
    pub(in crate::task) shared_pages: usize,
    /// 可执行用户 VMA 页数。
    pub(in crate::task) text_pages: usize,
    /// writable private data 与 stack VMA 页数。
    pub(in crate::task) data_pages: usize,
    /// Process fd table 当前 slot capacity。
    pub(in crate::task) fd_size: usize,
}

pub(in crate::task) struct AddressSpace {
    pub(super) memory_set: TaskMutex<MemorySet>,
    dumpability: Dumpability,
    // OWNER: mm 创建时的 user namespace；unshare 不改变，fork 继承，exec 重新取得。
    // non-dumpable ptrace 必须检查该 owner，不能以目标的当前 credential namespace 代替。
    user_namespace: Arc<crate::namespace::UserNamespace>,
    // CACHE: MemorySet 是 page-table root/ASID 的唯一 lifecycle owner；token 是该 owner
    // 生命周期内不变的 arch projection。trap_return 在 IRQ-disabled transfer window 读取它，
    // 若每次重新取得 memory_set lock，同 mm sibling 的 page fault 会把返回路径错误变成阻塞点。
    token: crate::arch::mmu::AddressSpaceToken,
    // OWNER: AddressSpace 唯一保存 private expedited membarrier registration；vfork/CLONE_VM
    // 共享同一状态，exec/fork 新 mm 从未注册开始。若放在 Process，shared-mm caller 会产生分裂状态。
    private_memory_barrier_registered: AtomicBool,
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 诊断不取得 mm lock，不扫描 VMA/设备，不影响映射 user 引用计数。
        formatter
            .debug_struct("AddressSpace")
            .finish_non_exhaustive()
    }
}

impl AddressSpace {
    /// 从 mm 的唯一 executable owner 取得实际 path；clone owner 后释放 MM guard，再做类型/资源访问。
    pub(super) fn executable(
        &self,
    ) -> Result<Option<Arc<crate::fs::OpenedFile>>, crate::fs::FileSystemError> {
        let owner = self
            .memory_set
            .lock()
            .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?
            .executable_owner();
        let Some(owner) = owner else {
            return Ok(None);
        };
        let owner: Arc<dyn core::any::Any + Send + Sync> = owner;
        let executable = owner
            .downcast::<super::super::loader::ExecutableOpenLease>()
            .unwrap_or_else(|_| panic!("user mm main executable is the actual loader open lease"));
        Ok(Some(executable.opened()))
    }

    /// Linux mm-owned dumpability，不读取或阻塞 MemorySet。
    pub(super) fn dumpable(&self) -> bool {
        self.dumpability.user_dumpable()
    }
    /// credentials commit 与 prctl 唯一更新同一个 mm scalar。
    pub(super) fn set_dumpable(&self, user: bool) {
        self.dumpability.set_user_dumpable(user);
    }

    /// mm 的 immutable user namespace，用于独立于当前 credentials 的 ptrace 授权。
    pub(super) fn user_namespace(&self) -> &Arc<crate::namespace::UserNamespace> {
        &self.user_namespace
    }

    pub(super) fn new(
        memory_set: MemorySet,
        user_namespace: Arc<crate::namespace::UserNamespace>,
    ) -> Result<Arc<Self>, MemoryError> {
        let token = memory_set.token();
        let owner = Arc::try_new(Self {
            memory_set: TaskMutex::new(memory_set),
            dumpability: Dumpability::new(true),
            user_namespace,
            token,
            private_memory_barrier_registered: AtomicBool::new(false),
        })
        .map_err(|_| MemoryError::OutOfMemory)?;
        crate::memory::register_memory_mapping_owner(owner.clone())
            .map_err(|_| MemoryError::OutOfMemory)?;
        crate::memory::register_memory_reclaimer(owner.clone())
            .map_err(|_| MemoryError::OutOfMemory)?;
        Ok(owner)
    }

    /// 在继承首个 page/backing owner 前登记空 child，消除裸 MemorySet 的 registry 空窗。
    /// child root/token 保持不变；COW 初始化期间两个 mm 由 task-only owner 保活，
    /// 后台 callback 只能 try_lock 跳过，其他注册表操作可等待当前完整初始化。
    ///
    /// # Errors
    ///
    /// 空 root、registry、waiter 或 COW preparation OOM 时丢弃未激活 child；parent 的
    /// 内容与 mapping identity 保持不变，已收紧的 COW leaf 可由同一 fault owner 恢复。
    pub(super) fn try_clone_for_fork(&self) -> Result<Arc<Self>, MemoryError> {
        let child = Self::new(MemorySet::try_new()?, self.user_namespace.clone())?;
        child
            .dumpability
            .set_user_dumpable(self.dumpability.user_dumpable());
        {
            let mut parent_memory = self
                .memory_set
                .lock()
                .map_err(|_| MemoryError::OutOfMemory)?;
            let mut child_memory = child
                .memory_set
                .lock()
                .map_err(|_| MemoryError::OutOfMemory)?;
            let result = parent_memory.try_clone_for_fork(&mut child_memory);
            // 完成/失败都在 guards 释放前发布 ownership-change generation；否则 swapoff
            // 可先扫空 child、再等待 parent，却漏掉随后继承到已扫 slot 的换出页。
            crate::memory::complete_memory_mapping_owner_initialization();
            result?;
        }
        Ok(child)
    }

    pub(super) fn page_statistics(&self) -> Result<crate::memory::UserPageStatistics, MemoryError> {
        Ok(self
            .memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?
            .user_page_statistics())
    }

    /// 按 Linux mm argument range 复制当前 Process 的实时 argv bytes。
    ///
    /// # Returns
    ///
    /// range 可读时返回 NUL 分隔 bytes。
    ///
    /// # Errors
    ///
    /// unmap/protection 或 kernel buffer OOM 返回精确 user-access 错误。
    pub(super) fn process_arguments(&self) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?
            .process_arguments()
    }
    pub(super) fn write_clone_tid_values(
        &self,
        addresses: [Option<usize>; 2],
        tid: i32,
        limits: UserFaultLimits,
    ) {
        let Ok(mut memory) = self.memory_set.lock() else {
            return;
        };
        super::clone_tid_store::store_clone_tid_values(addresses, |address| {
            memory.copy_to_user(address, &tid.to_ne_bytes(), limits)
        });
    }

    /// 在 AddressSpace owner lock 内验证 futex word 并生成稳定 key。
    ///
    /// # Parameters
    ///
    /// - `address`: 用户 futex 地址。
    /// - `private`: true 强制 address-space scope，false 允许共享 backing scope。
    /// - `consume`: 在 AddressSpace lock 内消费稳定 key 的闭包。
    ///
    /// # Returns
    ///
    /// 成功返回 memory-domain key；不可读映射返回 user access error。
    pub(super) fn with_futex_key<R>(
        &self,
        address: usize,
        private: bool,
        limits: UserFaultLimits,
        consume: impl FnOnce(FutexKey) -> R,
    ) -> Result<R, UserAccessError> {
        let identity = self as *const Self as usize;
        if private {
            return FutexKey::private(address, identity).map(consume);
        }
        let mut memory = self
            .memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?;
        let key = memory.futex_key(address, identity, private, limits)?;
        Ok(consume(key))
    }

    /// 在同一个 AddressSpace owner lock 内解析 futex key 并读取当前 word。
    ///
    /// # Parameters
    ///
    /// - `address`: 用户 futex 地址。
    /// - `private`: true 强制 address-space scope，false 允许共享 backing scope。
    /// - `consume`: 在锁内消费稳定 key 与当前 u32 value 的闭包。
    ///
    /// # Returns
    ///
    /// 成功返回闭包结果；不可读映射返回 user access error。
    pub(super) fn with_futex_word<R>(
        &self,
        address: usize,
        private: bool,
        limits: UserFaultLimits,
        consume: impl FnOnce(FutexKey, u32) -> R,
    ) -> Result<R, UserAccessError> {
        let identity = self as *const Self as usize;
        let mut memory = self
            .memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?;
        let key = memory.futex_key(address, identity, private, limits)?;
        let value = memory.load_user_u32(address, limits)?;
        Ok(consume(key, value))
    }

    /// 在同一个 AddressSpace owner lock 内解析两个 futex key 并读取 source word。
    ///
    /// # Parameters
    ///
    /// - `source`: source futex 用户地址。
    /// - `target`: target futex 用户地址。
    /// - `private`: true 强制 address-space scope，false 允许共享 backing scope。
    /// - `read_source`: 仅 CMP_REQUEUE 读取 source；普通 REQUEUE 的 private key 无需 VMA。
    /// - `consume`: 在锁内消费两个 key 与可选 source u32 value 的闭包。
    ///
    /// # Returns
    ///
    /// 成功返回闭包结果；任一映射不可读时返回 user access error。
    pub(super) fn with_futex_requeue<R>(
        &self,
        source: usize,
        target: usize,
        private: bool,
        limits: UserFaultLimits,
        read_source: bool,
        consume: impl FnOnce(FutexKey, FutexKey, Option<u32>) -> R,
    ) -> Result<R, crate::memory::FutexAccessError> {
        let identity = self as *const Self as usize;
        let mut memory = self
            .memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?;
        let source_key = memory.futex_key(source, identity, private, limits)?;
        if target & 3 != 0 {
            return Err(crate::memory::FutexAccessError::Unaligned);
        }
        let target_key = memory.futex_key(target, identity, private, limits)?;
        let value = if read_source {
            Some(memory.load_user_u32(source, limits)?)
        } else {
            None
        };
        Ok(consume(source_key, target_key, value))
    }

    /// 在实际 MM owner 内解析 READ/WRITE key；private key 不要求 VMA，实际 RMW 单独验证目标。
    /// callback 只供 task futex transaction 使用，memory guard 归还前不得调度已 claimed waiter。
    pub(super) fn with_futex_wake_op<R>(
        &self,
        source: usize,
        target: usize,
        private: bool,
        limits: UserFaultLimits,
        consume: impl FnOnce(FutexKey, FutexKey, &mut MemorySet) -> R,
    ) -> Result<R, crate::memory::FutexAccessError> {
        let identity = core::ptr::from_ref(self).addr();
        let mut memory = self
            .memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?;
        let source_key = memory.futex_key(source, identity, private, limits)?;
        if target & 3 != 0 {
            return Err(crate::memory::FutexAccessError::Unaligned);
        }
        if !private {
            memory.validate_user_write(target, 4, limits)?;
        }
        let target_key = memory.futex_key(target, identity, private, limits)?;
        Ok(consume(source_key, target_key, &mut memory))
    }

    pub(super) fn sync_shared_mapping(
        &self,
        address: usize,
        length: usize,
        writeback: bool,
    ) -> Result<(), MemoryError> {
        self.memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?
            .sync_shared_mapping(address, length, writeback)
    }

    pub(super) fn handle_page_fault(
        &self,
        address: usize,
        access: PageFaultAccess,
        limits: UserFaultLimits,
    ) -> Result<PageFaultOutcome, MemoryError> {
        self.memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?
            .handle_page_fault_with_limits(address, access, limits)
    }

    pub(super) fn unmap_user_mapping(
        &self,
        address: usize,
        length: usize,
    ) -> Result<(), MemoryError> {
        let retirement = {
            let mut memory = self
                .memory_set
                .lock()
                .map_err(|_| MemoryError::OutOfMemory)?;
            memory.prepare_user_unmap(address, length)?
        };
        retirement.synchronize();
        Ok(())
    }

    pub(super) fn protect_user_mapping(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        self.memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?
            .protect_user_mapping(address, length, permission)
    }

    pub(super) fn advise_user_mapping(
        &self,
        address: usize,
        length: usize,
        advice: crate::memory::MemoryAdvice,
    ) -> Result<(), MemoryError> {
        self.memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?
            .advise_user_mapping(address, length, advice)
    }

    /// 从用户地址空间复制字节到 kernel 缓冲区，地址空间锁覆盖整个复制。
    ///
    /// # Parameters
    ///
    /// - `user_address`: 用户源地址。
    /// - `destination`: kernel 目标缓冲区。
    ///
    /// # Returns
    ///
    /// 完整成功返回 `Ok(())`；fault、权限错误或 overflow 返回 `UserAccessError`。
    pub(super) fn copy_from_user(
        &self,
        user_address: usize,
        destination: &mut [u8],
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        self.memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?
            .copy_from_user(user_address, destination, limits)
    }

    pub(super) fn copy_from_user_uninit(
        &self,
        user_address: usize,
        destination: &mut [core::mem::MaybeUninit<u8>],
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        self.memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?
            .copy_from_user_uninit(user_address, destination, limits)
    }

    pub(super) fn copy_instruction_halfword(
        &self,
        user_address: usize,
        destination: &mut [u8; 2],
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        self.memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?
            .copy_instruction_halfword(user_address, destination, limits)
    }

    /// 将 kernel 缓冲区复制到用户地址空间，地址空间锁覆盖整个复制。
    ///
    /// # Parameters
    ///
    /// - `user_address`: 用户目标地址。
    /// - `source`: kernel 源缓冲区。
    ///
    /// # Returns
    ///
    /// 完整成功返回 `Ok(())`；fault、权限错误或 overflow 返回 `UserAccessError`。
    pub(super) fn copy_to_user(
        &self,
        user_address: usize,
        source: &[u8],
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        self.memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?
            .copy_to_user(user_address, source, limits)
    }

    /// 在单次 AddressSpace owner transaction 内 fault-in 并清零用户范围。
    pub(super) fn zero_user(
        &self,
        user_address: usize,
        length: usize,
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        self.memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?
            .zero_user(user_address, length, limits)
    }

    /// 在不修改内容的前提下准备并验证完整 userspace write range。
    ///
    /// # Parameters
    ///
    /// - `user_address`: 用户目标首地址。
    /// - `length`: 必须可写的 byte 数。
    /// - `limits`: fault-in 可消耗的资源上限。
    ///
    /// # Returns
    ///
    /// 完整范围可写返回 Ok；fault、权限或资源失败返回错误。
    pub(super) fn validate_user_write(
        &self,
        user_address: usize,
        length: usize,
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        self.memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?
            .validate_user_write(user_address, length, limits)
    }

    /// 从用户空间复制有上限的 NUL 结尾字节串。
    ///
    /// # Parameters
    ///
    /// - `user_address`: 用户字符串首地址。
    /// - `max_len`: 包含终止 NUL 的最大总字节数。
    ///
    /// # Returns
    ///
    /// 成功返回不含 NUL 的 owned bytes；fault、未终止或内存不足返回明确错误。
    pub(super) fn copy_user_c_string(
        &self,
        user_address: usize,
        max_len: usize,
        limits: UserFaultLimits,
    ) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?
            .copy_user_c_string(user_address, max_len, limits)
    }
}

impl MemoryMappingOwner for AddressSpace {
    fn revoke_swappable_page(
        &self,
        page: &crate::memory::SwappablePage,
        ppn: crate::memory::PhysicalPageNumber,
    ) -> Option<usize> {
        self.memory_set
            .try_lock()
            .map(|mut memory| memory.revoke_swappable_page(page, ppn))
    }
    fn revoke_shared_file_writers(
        &self,
        id: SharedFileId,
        wait: &mut crate::sync::TaskMutexWaitPreparation,
    ) {
        self.memory_set
            .lock_prepared(wait)
            .revoke_shared_file_writers(id);
    }
    fn restore_swapped_pages(
        &self,
        identity: crate::memory::SharedFileId,
        wait: &mut crate::sync::TaskMutexWaitPreparation,
    ) -> Result<(), crate::memory::SharedFileError> {
        self.memory_set
            .lock_prepared(wait)
            .restore_swapped_pages(identity)
            .map_err(|error| {
                if error.is_out_of_memory() {
                    crate::memory::SharedFileError::OutOfMemory
                } else {
                    crate::memory::SharedFileError::Io
                }
            })
    }

    fn invalidate_shared_file(
        &self,
        id: SharedFileId,
        size: u64,
        wait: &mut crate::sync::TaskMutexWaitPreparation,
    ) {
        self.memory_set
            .lock_prepared(wait)
            .invalidate_shared_file(id, size);
    }
}

impl MemoryReclaimer for AddressSpace {
    fn reclaim_pages(&self, request: ReclaimRequest) -> ReclaimResult {
        crate::memory::reclaim_address_space(&self.memory_set, request)
    }
}
