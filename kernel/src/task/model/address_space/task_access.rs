use super::*;
use core::sync::atomic::Ordering;

impl Process {
    /// @description 取得当前 Process 唯一 AddressSpace handle 的保活引用。
    pub(in crate::task::model) fn address_space(&self) -> Arc<AddressSpace> {
        self.address_space.lock().clone()
    }

    /// @description 在 exec commit point 原子替换 AddressSpace handle。
    /// @return 替换前的 owner。
    pub(in crate::task::model) fn replace_address_space(
        &self,
        replacement: Arc<AddressSpace>,
    ) -> Arc<AddressSpace> {
        core::mem::replace(&mut *self.address_space.lock(), replacement)
    }
}

impl TaskControlBlock {
    pub(in crate::task) fn register_private_memory_barrier(&self) {
        self.process
            .address_space()
            .private_memory_barrier_registered
            .store(true, Ordering::Release);
    }

    pub(in crate::task) fn private_memory_barrier_registered(&self) -> bool {
        self.process
            .address_space()
            .private_memory_barrier_registered
            .load(Ordering::Acquire)
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

    /// @description 直接初始化 syscall-owned staging；成功前 caller 不得读取 destination。
    pub(crate) fn copy_from_user_uninit(
        &self,
        user_address: usize,
        destination: &mut [core::mem::MaybeUninit<u8>],
    ) -> Result<(), UserAccessError> {
        self.process.address_space().copy_from_user_uninit(
            user_address,
            destination,
            self.user_fault_limits(),
        )
    }

    pub(crate) fn copy_instruction_halfword(
        &self,
        user_address: usize,
        destination: &mut [u8; 2],
    ) -> Result<(), UserAccessError> {
        self.process.address_space().copy_instruction_halfword(
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

    pub(crate) fn zero_user(
        &self,
        user_address: usize,
        length: usize,
    ) -> Result<(), UserAccessError> {
        self.process
            .address_space()
            .zero_user(user_address, length, self.user_fault_limits())
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

    pub(in crate::task) fn write_clone_tid_values(&self, addresses: [Option<usize>; 2], tid: i32) {
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

    /// @description 返回当前 AddressSpace 生命周期内不变的 arch token。
    pub(crate) fn user_token(&self) -> crate::arch::mmu::AddressSpaceToken {
        self.process.address_space().token
    }

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

    pub(in crate::task) fn process_arguments(
        &self,
    ) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.process.address_space().process_arguments()
    }

    /// @description 从 Process 与 AddressSpace owner 取得一次 procfs 统计快照。
    /// @errors comm 或 task-mutex waiter storage OOM 时返回错误。
    pub(in crate::task) fn process_statistics(&self) -> Result<ProcessStatistics, ()> {
        let (virtual_pages, resident_pages, shared_pages, text_pages, data_pages) = self
            .process
            .address_space()
            .page_statistics()
            .map_err(|_| ())?;
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
