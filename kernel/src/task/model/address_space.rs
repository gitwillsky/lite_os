use super::*;
use crate::memory::{ReclaimRequest, ReclaimResult};
use core::sync::atomic::{AtomicBool, Ordering};

mod mapping;

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

#[derive(Debug)]
pub(super) struct AddressSpace {
    pub(super) memory_set: Mutex<MemorySet>,
    // OWNER: AddressSpace 唯一保存 private expedited membarrier registration；vfork/CLONE_VM
    // 共享同一状态，exec/fork 新 mm 从未注册开始。若放在 Process，shared-mm caller 会产生分裂状态。
    private_memory_barrier_registered: AtomicBool,
}

impl AddressSpace {
    pub(super) fn new(memory_set: MemorySet) -> Result<Arc<Self>, MemoryError> {
        let owner = Arc::try_new(Self {
            memory_set: Mutex::new(memory_set),
            private_memory_barrier_registered: AtomicBool::new(false),
        })
        .map_err(|_| MemoryError::OutOfMemory)?;
        crate::memory::register_memory_mapping_owner(owner.clone())
            .map_err(|_| MemoryError::OutOfMemory)?;
        crate::memory::register_memory_reclaimer(owner.clone())
            .map_err(|_| MemoryError::OutOfMemory)?;
        Ok(owner)
    }
    pub(super) fn page_statistics(&self) -> (usize, usize, usize, usize, usize) {
        self.memory_set.lock().user_page_statistics()
    }

    /// @description 按 Linux mm argument range 复制当前 Process 的实时 argv bytes。
    /// @return range 可读时返回 NUL 分隔 bytes。
    /// @errors unmap/protection 或 kernel buffer OOM 返回精确 user-access 错误。
    pub(super) fn process_arguments(&self) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.memory_set.lock().process_arguments()
    }
    pub(super) fn write_clone_tid_values(
        &self,
        addresses: [Option<usize>; 2],
        tid: i32,
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        let mut memory = self.memory_set.lock();
        for address in addresses.into_iter().flatten() {
            memory.validate_user_write(address, core::mem::size_of::<i32>(), limits)?;
        }
        for address in addresses.into_iter().flatten() {
            memory.copy_to_user(address, &tid.to_ne_bytes(), limits)?;
        }
        Ok(())
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
        self.memory_set.lock().map_anonymous(
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
        self.memory_set.lock().map_shared_anonymous(
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
        let mut memory = self.memory_set.lock();
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
        let mut memory = self.memory_set.lock();
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
        let mut memory = self.memory_set.lock();
        let source_key = memory.futex_key(source, identity, private, limits)?;
        let target_key = memory.futex_key(target, identity, private, limits)?;
        let mut bytes = [0u8; core::mem::size_of::<u32>()];
        memory.copy_from_user(source, &mut bytes, limits)?;
        Ok(consume(source_key, target_key, u32::from_ne_bytes(bytes)))
    }

    pub(super) fn map_private_file(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        file: FileMappingSource,
        limits: MappingResourceLimits,
    ) -> Result<usize, MemoryError> {
        self.memory_set.lock().map_private_file(
            address,
            length,
            permission,
            fixed_noreplace,
            file,
            limits,
        )
    }

    pub(super) fn map_shared_file(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        file: FileMappingSource,
        address_space_limit: u64,
    ) -> Result<usize, MemoryError> {
        self.memory_set.lock().map_shared_file(
            address,
            length,
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
            .handle_page_fault_with_limits(address, access, limits)
    }

    pub(super) fn unmap_user_mapping(
        &self,
        address: usize,
        length: usize,
    ) -> Result<(), MemoryError> {
        self.memory_set.lock().unmap_user_mapping(address, length)
    }

    pub(super) fn protect_user_mapping(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        self.memory_set
            .lock()
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
            .copy_from_user(user_address, destination, limits)
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
            .copy_to_user(user_address, source, limits)
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
            .copy_user_c_string(user_address, max_len, limits)
    }
}

impl Process {
    /// @description 取得当前 Process 唯一 AddressSpace handle 的保活引用。
    /// @return clone 后的 Arc；handle lock 不跨 memory operation 持有。
    pub(super) fn address_space(&self) -> Arc<AddressSpace> {
        self.address_space.lock().clone()
    }

    /// @description 在 exec point-of-no-return 原子替换当前 Process 的 AddressSpace handle。
    ///
    /// @param replacement 已完整构造且可直接运行的新 AddressSpace。
    /// @return 替换前的 owner；vfork child 用它删除共享 mm 内的临时 trap-context 页。
    pub(super) fn replace_address_space(
        &self,
        replacement: Arc<AddressSpace>,
    ) -> Arc<AddressSpace> {
        core::mem::replace(&mut *self.address_space.lock(), replacement)
    }
}

impl TaskControlBlock {
    /// @description 为当前 AddressSpace 单调注册 private expedited memory barrier。
    ///
    /// @return 无返回值；重复注册保持成功。
    pub(in crate::task) fn register_private_memory_barrier(&self) {
        self.process
            .address_space()
            .private_memory_barrier_registered
            .store(true, Ordering::Release);
    }

    /// @description 查询当前 AddressSpace 是否已注册 private expedited memory barrier。
    ///
    /// @return 当前 mm 已完成注册时返回 true。
    pub(in crate::task) fn private_memory_barrier_registered(&self) -> bool {
        self.process
            .address_space()
            .private_memory_barrier_registered
            .load(Ordering::Acquire)
    }

    /// @description 删除非 canonical 的 Thread/vfork-child supervisor trap-context 页。
    /// @return 无返回值；canonical process trap context 随 AddressSpace 生命周期释放。
    pub(in crate::task) fn remove_thread_trap_context(&self) {
        let address = self.trap_context_va();
        if address == TRAP_CONTEXT {
            return;
        }
        self.process
            .address_space()
            .memory_set
            .lock()
            .remove_thread_trap_context(address);
    }

    /// @description 返回当前 Thread 的 supervisor trap-context 虚拟地址。
    /// @return canonical Process 地址或共享 mm 内按 TID 分配的独立地址。
    pub(crate) fn trap_context_va(&self) -> usize {
        *self.thread.trap_cx_va.lock()
    }

    /// @description 覆盖当前 Thread 的 supervisor-only trap context。
    ///
    /// @param trap_context 待写入的完整上下文值。
    /// @return 无返回值；映射缺失表示 kernel 不变量损坏并 panic。
    pub(crate) fn set_trap_context(&self, trap_context: TrapContext) {
        let va = self.trap_context_va();
        let address_space = self.process.address_space();
        let memory_set = address_space.memory_set.lock();
        let ppn = memory_set.trap_context_ppn(va);
        let offset = VirtualAddress::from(va).page_offset();
        assert!(offset + core::mem::size_of::<TrapContext>() <= crate::memory::PAGE_SIZE);
        // SAFETY: validated page offset keeps pointer arithmetic inside the live trap-context
        // frame retained by the address-space guard.
        let ptr = unsafe { ppn.as_page_mut_ptr().add(offset).cast::<TrapContext>() };
        assert!(
            ptr.is_aligned(),
            "TrapContext physical address is not aligned"
        );
        // SAFETY: address-space guard 保证映射存活；当前 Thread 是该 trap context 的唯一写者。
        unsafe { ptr.write(trap_context) };
    }

    /// @description 复制当前 Thread trap context，不让底层映射引用逃逸地址空间锁。
    /// @return owned TrapContext clone；映射缺失表示 kernel 不变量损坏并 panic。
    pub(crate) fn load_trap_context(&self) -> TrapContext {
        let va = self.trap_context_va();
        let address_space = self.process.address_space();
        let memory_set = address_space.memory_set.lock();
        let ppn = memory_set.trap_context_ppn(va);
        let offset = VirtualAddress::from(va).page_offset();
        assert!(offset + core::mem::size_of::<TrapContext>() <= crate::memory::PAGE_SIZE);
        // SAFETY: validated page offset keeps pointer arithmetic inside the live trap-context
        // frame retained by the address-space guard.
        let ptr = unsafe { ppn.as_page_ptr().add(offset).cast::<TrapContext>() };
        assert!(
            ptr.is_aligned(),
            "TrapContext physical address is not aligned"
        );
        // SAFETY: guard 保证 frame 存活；只读引用仅用于本行 clone 且不会逃逸。
        unsafe { (&*ptr).clone() }
    }

    pub(crate) fn copy_from_user(
        &self,
        user_address: usize,
        destination: &mut [u8],
    ) -> Result<(), UserAccessError> {
        self.process.address_space().copy_from_user(
            user_address,
            destination,
            self.user_fault_limits(),
        )
    }

    pub(crate) fn copy_to_user(
        &self,
        user_address: usize,
        source: &[u8],
    ) -> Result<(), UserAccessError> {
        self.process
            .address_space()
            .copy_to_user(user_address, source, self.user_fault_limits())
    }

    pub(crate) fn validate_user_write(
        &self,
        user_address: usize,
        length: usize,
    ) -> Result<(), UserAccessError> {
        self.process.address_space().validate_user_write(
            user_address,
            length,
            self.user_fault_limits(),
        )
    }

    pub(in crate::task) fn write_clone_tid_values(
        &self,
        addresses: [Option<usize>; 2],
        tid: i32,
    ) -> Result<(), UserAccessError> {
        self.process.address_space().write_clone_tid_values(
            addresses,
            tid,
            self.user_fault_limits(),
        )
    }

    pub(crate) fn copy_user_c_string(
        &self,
        user_address: usize,
        max_len: usize,
    ) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.process.address_space().copy_user_c_string(
            user_address,
            max_len,
            self.user_fault_limits(),
        )
    }

    pub(crate) fn user_token(&self) -> usize {
        self.process.address_space().memory_set.lock().token()
    }

    /// @description 使用当前 Process AddressSpace 解析并消费 futex key。
    ///
    /// @param address 用户 futex 地址。
    /// @param private 是否强制 address-space scope。
    /// @param consume 在 AddressSpace lock 内消费 key 的闭包。
    /// @return 成功返回闭包结果；地址不可读返回 user access error。
    pub(crate) fn with_futex_key<R>(
        &self,
        address: usize,
        private: bool,
        consume: impl FnOnce(FutexKey) -> R,
    ) -> Result<R, UserAccessError> {
        self.process.address_space().with_futex_key(
            address,
            private,
            self.user_fault_limits(),
            consume,
        )
    }

    /// @description 使用当前 Process AddressSpace 原子解析 futex key 与当前 word。
    ///
    /// @param address 用户 futex 地址。
    /// @param private 是否强制 address-space scope。
    /// @param consume 在 AddressSpace lock 内消费 key 与 u32 value 的闭包。
    /// @return 成功返回闭包结果；地址不可读返回 user access error。
    pub(crate) fn with_futex_word<R>(
        &self,
        address: usize,
        private: bool,
        consume: impl FnOnce(FutexKey, u32) -> R,
    ) -> Result<R, UserAccessError> {
        self.process.address_space().with_futex_word(
            address,
            private,
            self.user_fault_limits(),
            consume,
        )
    }

    /// @description 使用当前 Process AddressSpace 原子解析 requeue 两端 key 与 source word。
    ///
    /// @param source source futex 用户地址。
    /// @param target target futex 用户地址。
    /// @param private 是否强制 address-space scope。
    /// @param consume 在 AddressSpace lock 内消费两个 key 与 source u32 value 的闭包。
    /// @return 成功返回闭包结果；任一地址不可读返回 user access error。
    pub(crate) fn with_futex_requeue<R>(
        &self,
        source: usize,
        target: usize,
        private: bool,
        consume: impl FnOnce(FutexKey, FutexKey, u32) -> R,
    ) -> Result<R, UserAccessError> {
        self.process.address_space().with_futex_requeue(
            source,
            target,
            private,
            self.user_fault_limits(),
            consume,
        )
    }

    /// @description 通过 Process address-space owner 读取实时 argv bytes。
    /// @return argument range 可读时返回 NUL 分隔 bytes。
    /// @errors 用户映射不可读或 kernel buffer OOM 时返回错误。
    pub(in crate::task) fn process_arguments(
        &self,
    ) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.process.address_space().process_arguments()
    }

    /// @description 从 Process 与 AddressSpace 唯一 owner 取得一次 procfs 统计快照。
    /// @return comm、创建时刻、Linux statm 页统计与 fd table capacity。
    /// @errors comm snapshot storage OOM 时返回错误。
    pub(in crate::task) fn process_statistics(&self) -> Result<ProcessStatistics, ()> {
        let (virtual_pages, resident_pages, shared_pages, text_pages, data_pages) =
            self.process.address_space().page_statistics();
        let comm = self.process.comm.lock();
        let mut comm_snapshot = alloc::vec::Vec::new();
        comm_snapshot
            .try_reserve_exact(comm.len())
            .map_err(|_| ())?;
        comm_snapshot.extend_from_slice(&comm);
        Ok(ProcessStatistics {
            comm: comm_snapshot,
            start_time_us: self.process.start_time_us,
            virtual_pages,
            resident_pages,
            shared_pages,
            text_pages,
            data_pages,
            fd_size: self.process.files.lock().slot_capacity(),
        })
    }
}

impl MemoryMappingOwner for AddressSpace {
    fn invalidate_shared_file(&self, id: SharedFileId, size: u64) {
        self.memory_set.lock().invalidate_shared_file(id, size);
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
