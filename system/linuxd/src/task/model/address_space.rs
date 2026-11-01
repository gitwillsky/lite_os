use super::*;
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
    // OWNER: AddressSpace 唯一保存 private expedited membarrier registration；vfork/CLONE_VM
    // 共享同一状态，exec/fork 新 mm 从未注册开始。
    private_memory_barrier_registered: AtomicBool,
    // CACHE: 承载本地址空间的成员进程；与 MemorySet 同寿命且不变，缓存它使创建承载线程不必取得 mm 锁。
    process: Arc<native_runtime::Process>,
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("AddressSpace").finish_non_exhaustive()
    }
}

impl AddressSpace {
    fn lock(&self) -> Result<crate::sync::TaskMutexGuard<'_, MemorySet>, MemoryError> {
        self.memory_set.lock().map_err(|_| MemoryError::OutOfMemory)
    }

    fn lock_for_access(&self) -> Result<crate::sync::TaskMutexGuard<'_, MemorySet>, UserAccessError> {
        self.memory_set.lock().map_err(|_| UserAccessError::OutOfMemory)
    }

    /// 承载本地址空间的成员进程。
    pub(in crate::task) fn process(&self) -> &Arc<native_runtime::Process> {
        &self.process
    }

    /// 主程序打开租约（`/proc/<pid>/exe`）。
    pub(super) fn executable(&self) -> Result<Option<Arc<crate::fs::OpenedFile>>, crate::fs::FileSystemError> {
        let owner = self
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

    pub(super) fn dumpable(&self) -> bool {
        self.dumpability.user_dumpable()
    }

    pub(super) fn set_dumpable(&self, user: bool) {
        self.dumpability.set_user_dumpable(user);
    }

    pub(super) fn user_namespace(&self) -> &Arc<crate::namespace::UserNamespace> {
        &self.user_namespace
    }

    pub(super) fn new(memory_set: MemorySet, user_namespace: Arc<crate::namespace::UserNamespace>) -> Result<Arc<Self>, MemoryError> {
        let process = memory_set.process().clone();
        Arc::try_new(Self {
            memory_set: TaskMutex::new(memory_set),
            dumpability: Dumpability::new(true),
            user_namespace,
            private_memory_barrier_registered: AtomicBool::new(false),
            process,
        })
        .map_err(|_| MemoryError::OutOfMemory)
    }

    /// fork：子地址空间是共享进程的新成员，私有映射取快照。
    ///
    /// # Errors
    ///
    /// 成员进程或快照创建失败；父空间不变。
    pub(super) fn try_clone_for_fork(&self, name: &[u8]) -> Result<Arc<Self>, MemoryError> {
        let child = self.lock()?.try_clone_for_fork(name)?;
        let child = Self::new(child, self.user_namespace.clone())?;
        child.dumpability.set_user_dumpable(self.dumpability.user_dumpable());
        Ok(child)
    }

    pub(super) fn page_statistics(&self) -> Result<crate::memory::UserPageStatistics, MemoryError> {
        Ok(self.lock()?.user_page_statistics())
    }

    pub(super) fn process_arguments(&self) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.lock_for_access()?.process_arguments()
    }

    pub(super) fn write_clone_tid_values(&self, addresses: [Option<usize>; 2], tid: i32) {
        let Ok(memory) = self.memory_set.lock() else {
            return;
        };
        super::clone_tid_store::store_clone_tid_values(addresses, |address| memory.copy_to_user(address, &tid.to_ne_bytes()));
    }

    /// 在地址空间锁内生成 futex key。
    pub(super) fn with_futex_key<R>(
        &self,
        address: usize,
        private: bool,
        consume: impl FnOnce(FutexKey) -> R,
    ) -> Result<R, UserAccessError> {
        let identity = self as *const Self as usize;
        if private {
            return FutexKey::private(address, identity).map(consume);
        }
        let memory = self.lock_for_access()?;
        let key = memory.futex_key(address, identity, private)?;
        Ok(consume(key))
    }

    /// 在地址空间锁内生成 futex key 并读取当前值。
    pub(super) fn with_futex_word<R>(
        &self,
        address: usize,
        private: bool,
        consume: impl FnOnce(FutexKey, u32) -> R,
    ) -> Result<R, UserAccessError> {
        let identity = self as *const Self as usize;
        let memory = self.lock_for_access()?;
        let key = memory.futex_key(address, identity, private)?;
        let value = memory.load_user_u32(address)?;
        Ok(consume(key, value))
    }

    /// 在地址空间锁内生成两个 futex key 并（CMP_REQUEUE 时）读取源值。
    pub(super) fn with_futex_requeue<R>(
        &self,
        source: usize,
        target: usize,
        private: bool,
        read_source: bool,
        consume: impl FnOnce(FutexKey, FutexKey, Option<u32>) -> R,
    ) -> Result<R, crate::memory::FutexAccessError> {
        let identity = self as *const Self as usize;
        let memory = self.lock_for_access()?;
        let source_key = memory.futex_key(source, identity, private)?;
        if target & 3 != 0 {
            return Err(crate::memory::FutexAccessError::Unaligned);
        }
        let target_key = memory.futex_key(target, identity, private)?;
        let value = if read_source { Some(memory.load_user_u32(source)?) } else { None };
        Ok(consume(source_key, target_key, value))
    }

    /// FUTEX_WAKE_OP：在地址空间锁内生成两个 key，并把锁内的地址空间交给原子更新。
    pub(super) fn with_futex_wake_op<R>(
        &self,
        source: usize,
        target: usize,
        private: bool,
        consume: impl FnOnce(FutexKey, FutexKey, &MemorySet) -> R,
    ) -> Result<R, crate::memory::FutexAccessError> {
        let identity = core::ptr::from_ref(self).addr();
        let memory = self.lock_for_access()?;
        let source_key = memory.futex_key(source, identity, private)?;
        if target & 3 != 0 {
            return Err(crate::memory::FutexAccessError::Unaligned);
        }
        if !private {
            memory.validate_user_write(target, 4)?;
        }
        let target_key = memory.futex_key(target, identity, private)?;
        Ok(consume(source_key, target_key, &memory))
    }

    /// 信号返回跳板地址（不变，免锁读取缓存值）。
    pub(super) fn signal_trampoline(&self) -> usize {
        crate::memory::user_address_end() - crate::memory::PAGE_SIZE
    }

    pub(super) fn sync_shared_mapping(&self, address: usize, length: usize, writeback: bool) -> Result<(), MemoryError> {
        self.lock()?.sync_shared_mapping(address, length, writeback)
    }

    pub(super) fn handle_page_fault(&self, address: usize, access: PageFaultAccess) -> Result<PageFaultOutcome, MemoryError> {
        self.lock()?.handle_page_fault(address, access)
    }

    pub(super) fn unmap_user_mapping(&self, address: usize, length: usize) -> Result<(), MemoryError> {
        self.lock()?.unmap_user_mapping(address, length)
    }

    pub(super) fn protect_user_mapping(&self, address: usize, length: usize, permission: MapPermission) -> Result<(), MemoryError> {
        self.lock()?.protect_user_mapping(address, length, permission)
    }

    pub(super) fn advise_user_mapping(&self, address: usize, length: usize, advice: crate::memory::MemoryAdvice) -> Result<(), MemoryError> {
        self.lock()?.advise_user_mapping(address, length, advice)
    }

    pub(super) fn copy_from_user(&self, user_address: usize, destination: &mut [u8]) -> Result<(), UserAccessError> {
        self.lock_for_access()?.copy_from_user(user_address, destination)
    }

    /// 复制到未初始化的缓冲区：先清零再复制，成功后整个缓冲区已初始化。
    pub(super) fn copy_from_user_uninit(
        &self,
        user_address: usize,
        destination: &mut [core::mem::MaybeUninit<u8>],
    ) -> Result<(), UserAccessError> {
        for byte in destination.iter_mut() {
            byte.write(0);
        }
        // SAFETY: 上面已初始化每个字节；`MaybeUninit<u8>` 与 `u8` 布局相同。
        let initialized = unsafe { &mut *(core::ptr::from_mut(destination) as *mut [u8]) };
        self.copy_from_user(user_address, initialized)
    }

    pub(super) fn copy_to_user(&self, user_address: usize, source: &[u8]) -> Result<(), UserAccessError> {
        self.lock_for_access()?.copy_to_user(user_address, source)
    }

    pub(super) fn zero_user(&self, user_address: usize, length: usize) -> Result<(), UserAccessError> {
        self.lock_for_access()?.zero_user(user_address, length)
    }

    pub(super) fn validate_user_write(&self, user_address: usize, length: usize) -> Result<(), UserAccessError> {
        self.lock_for_access()?.validate_user_write(user_address, length)
    }

    pub(super) fn copy_user_c_string(&self, user_address: usize, max_len: usize) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.lock_for_access()?.copy_user_c_string(user_address, max_len)
    }

    /// 调试访问（ptrace、`/proc/<pid>/mem`）：返回实际传输字节数。
    pub(super) fn access_forced(&self, address: usize, buffer: &mut [u8], write: bool) -> Result<usize, UserAccessError> {
        Ok(self.lock_for_access()?.access_forced(address, buffer, write))
    }
}
