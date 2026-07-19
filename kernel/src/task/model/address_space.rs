use super::*;
use crate::memory::{ReclaimRequest, ReclaimResult};
use core::sync::atomic::AtomicBool;

mod mapping;
mod task_access;

/// @description Process owner 向 task façade 发布的 procfs 统计快照。
pub(in crate::task) struct ProcessStatistics {
    /// Process 共享 comm。
    pub(in crate::task) comm: Vec<u8>,
    /// Process 创建时的 monotonic 微秒时间。
    pub(in crate::task) start_time_us: u64,
    /// AddressSpace 用户 VMA 总页数。
    pub(in crate::task) virtual_pages: usize,
    /// AddressSpace 当前驻留页数。
    pub(in crate::task) resident_pages: usize,
    /// 当前驻留且由共享 mapping owner 持有的页数。
    pub(in crate::task) shared_pages: usize,
    /// 可执行用户 VMA 页数。
    pub(in crate::task) text_pages: usize,
    /// writable private data 与 stack VMA 页数。
    pub(in crate::task) data_pages: usize,
    /// Process fd table 当前 slot capacity。
    pub(in crate::task) fd_size: usize,
}

pub(super) struct AddressSpace {
    pub(super) memory_set: TaskMutex<MemorySet>,
    // CACHE: MemorySet 是 page-table root/ASID 的唯一 lifecycle owner；token 是该 owner
    // 生命周期内不变的 arch projection。trap_return 在 IRQ-disabled transfer window 读取它，
    // 若每次重新取得 memory_set lock，同 mm sibling 的 page fault 会把返回路径错误变成阻塞点。
    token: crate::arch::mmu::AddressSpaceToken,
    // OWNER: AddressSpace 唯一保存 private expedited membarrier registration；vfork/CLONE_VM
    // 共享同一状态，exec/fork 新 mm 从未注册开始。若放在 Process，shared-mm caller 会产生分裂状态。
    private_memory_barrier_registered: AtomicBool,
}

impl AddressSpace {
    pub(super) fn new(memory_set: MemorySet) -> Result<Arc<Self>, MemoryError> {
        let token = memory_set.token();
        let owner = Arc::try_new(Self {
            memory_set: TaskMutex::new(memory_set),
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

    pub(super) fn page_statistics(
        &self,
    ) -> Result<(usize, usize, usize, usize, usize), MemoryError> {
        Ok(self
            .memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?
            .user_page_statistics())
    }

    /// @description 按 Linux mm argument range 复制当前 Process 的实时 argv bytes。
    /// @return range 可读时返回 NUL 分隔 bytes。
    /// @errors unmap/protection 或 kernel buffer OOM 返回精确 user-access 错误。
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

    pub(super) fn map_anonymous(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<usize, MemoryError> {
        self.memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?
            .map_anonymous(
                address,
                length,
                permission,
                fixed_noreplace,
                address_space_limit,
                data_limit,
            )
    }

    /// @description 在 AddressSpace owner 下建立唯一 anonymous shared mapping。
    ///
    /// @param address 零为内核选址，非零为 hint 或 fixed_noreplace exact address。
    /// @param length 非零 mapping 字节长度。
    /// @param permission 用户页权限。
    /// @param fixed_noreplace 是否禁止覆盖已有 VMA。
    /// @return 成功返回 mapping 起点；非法范围、冲突或内存不足返回 MemoryError。
    pub(super) fn map_shared_anonymous(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        address_space_limit: u64,
    ) -> Result<usize, MemoryError> {
        self.memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?
            .map_shared_anonymous(
                address,
                length,
                permission,
                fixed_noreplace,
                address_space_limit,
            )
    }

    /// @description 在 AddressSpace owner lock 内验证 futex word 并生成稳定 key。
    ///
    /// @param address 用户 futex 地址。
    /// @param private true 强制 address-space scope，false 允许共享 backing scope。
    /// @param consume 在 AddressSpace lock 内消费稳定 key 的闭包。
    /// @return 成功返回 memory-domain key；不可读映射返回 user access error。
    pub(super) fn with_futex_key<R>(
        &self,
        address: usize,
        private: bool,
        limits: UserFaultLimits,
        consume: impl FnOnce(FutexKey) -> R,
    ) -> Result<R, UserAccessError> {
        let identity = self as *const Self as usize;
        let mut memory = self
            .memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?;
        let key = memory.futex_key(address, identity, private, limits)?;
        Ok(consume(key))
    }

    /// @description 在同一个 AddressSpace owner lock 内解析 futex key 并读取当前 word。
    ///
    /// @param address 用户 futex 地址。
    /// @param private true 强制 address-space scope，false 允许共享 backing scope。
    /// @param consume 在锁内消费稳定 key 与当前 u32 value 的闭包。
    /// @return 成功返回闭包结果；不可读映射返回 user access error。
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
        let mut bytes = [0u8; core::mem::size_of::<u32>()];
        memory.copy_from_user(address, &mut bytes, limits)?;
        Ok(consume(key, u32::from_ne_bytes(bytes)))
    }

    /// @description 在同一个 AddressSpace owner lock 内解析两个 futex key 并读取 source word。
    ///
    /// @param source source futex 用户地址。
    /// @param target target futex 用户地址。
    /// @param private true 强制 address-space scope，false 允许共享 backing scope。
    /// @param consume 在锁内消费两个 key 与 source u32 value 的闭包。
    /// @return 成功返回闭包结果；任一映射不可读时返回 user access error。
    pub(super) fn with_futex_requeue<R>(
        &self,
        source: usize,
        target: usize,
        private: bool,
        limits: UserFaultLimits,
        consume: impl FnOnce(FutexKey, FutexKey, u32) -> R,
    ) -> Result<R, UserAccessError> {
        let identity = self as *const Self as usize;
        let mut memory = self
            .memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?;
        let source_key = memory.futex_key(source, identity, private, limits)?;
        let target_key = memory.futex_key(target, identity, private, limits)?;
        let mut bytes = [0u8; core::mem::size_of::<u32>()];
        memory.copy_from_user(source, &mut bytes, limits)?;
        Ok(consume(source_key, target_key, u32::from_ne_bytes(bytes)))
    }

    pub(super) fn map_private_file(
        &self,
        address: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        file: FileMappingSource,
        limits: MappingResourceLimits,
    ) -> Result<usize, MemoryError> {
        self.memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?
            .map_private_file(address, permission, fixed_noreplace, file, limits)
    }

    pub(super) fn map_shared_file(
        &self,
        address: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        file: FileMappingSource,
        address_space_limit: u64,
    ) -> Result<usize, MemoryError> {
        self.memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?
            .map_shared_file(
                address,
                permission,
                fixed_noreplace,
                file,
                address_space_limit,
            )
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
        self.memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?
            .unmap_user_mapping(address, length)
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

    /// @description 从用户地址空间复制字节到 kernel 缓冲区，地址空间锁覆盖整个复制。
    ///
    /// @param user_address 用户源地址。
    /// @param destination kernel 目标缓冲区。
    /// @return 完整成功返回 `Ok(())`；fault、权限错误或 overflow 返回 `UserAccessError`。
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

    /// @description 将 kernel 缓冲区复制到用户地址空间，地址空间锁覆盖整个复制。
    ///
    /// @param user_address 用户目标地址。
    /// @param source kernel 源缓冲区。
    /// @return 完整成功返回 `Ok(())`；fault、权限错误或 overflow 返回 `UserAccessError`。
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

    /// @description 在单次 AddressSpace owner transaction 内 fault-in 并清零用户范围。
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

    /// @description 在不修改内容的前提下准备并验证完整 userspace write range。
    /// @param user_address 用户目标首地址。
    /// @param length 必须可写的 byte 数。
    /// @param limits fault-in 可消耗的资源上限。
    /// @return 完整范围可写返回 Ok；fault、权限或资源失败返回错误。
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

    /// @description 从用户空间复制有上限的 NUL 结尾字节串。
    ///
    /// @param user_address 用户字符串首地址。
    /// @param max_len 包含终止 NUL 的最大总字节数。
    /// @return 成功返回不含 NUL 的 owned bytes；fault、未终止或内存不足返回明确错误。
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
        self.memory_set
            .try_lock()
            .map_or_else(ReclaimResult::default, |mut memory| {
                memory.reclaim_private_pages(request)
            })
    }
}
