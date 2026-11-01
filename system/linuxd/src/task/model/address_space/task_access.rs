use super::*;
use core::sync::atomic::Ordering;

// io_uring worker 可持有已退出 Thread 的 observer；fallible user-copy 必须先取得实际 mmget。
// 退休后返回 Fault，不能把仍可 upgrade 的 TCB Arc 当成 MM 存活证明，否则后台 completion 会 panic。

impl TaskControlBlock {
    pub(in crate::execution::task) fn register_private_memory_barrier(&self) {
        self.address_space()
            .private_memory_barrier_registered
            .store(true, Ordering::Release);
    }

    pub(in crate::execution::task) fn private_memory_barrier_registered(&self) -> bool {
        self.address_space()
            .private_memory_barrier_registered
            .load(Ordering::Acquire)
    }

    pub(crate) fn copy_from_user(
        &self,
        user_address: usize,
        destination: &mut [u8],
    ) -> Result<(), UserAccessError> {
        self.mm_snapshot()
            .ok_or(UserAccessError::Fault)?
            .copy_from_user(user_address, destination, self.user_fault_limits())
    }

    /// 直接初始化 syscall-owned staging；成功前 caller 不得读取 destination。
    pub(crate) fn copy_from_user_uninit(
        &self,
        user_address: usize,
        destination: &mut [core::mem::MaybeUninit<u8>],
    ) -> Result<(), UserAccessError> {
        self.mm_snapshot()
            .ok_or(UserAccessError::Fault)?
            .copy_from_user_uninit(user_address, destination, self.user_fault_limits())
    }

    pub(crate) fn copy_instruction_halfword(
        &self,
        user_address: usize,
        destination: &mut [u8; 2],
    ) -> Result<(), UserAccessError> {
        self.mm_snapshot()
            .ok_or(UserAccessError::Fault)?
            .copy_instruction_halfword(user_address, destination, self.user_fault_limits())
    }

    pub(crate) fn copy_to_user(
        &self,
        user_address: usize,
        source: &[u8],
    ) -> Result<(), UserAccessError> {
        self.mm_snapshot()
            .ok_or(UserAccessError::Fault)?
            .copy_to_user(user_address, source, self.user_fault_limits())
    }

    pub(crate) fn zero_user(
        &self,
        user_address: usize,
        length: usize,
    ) -> Result<(), UserAccessError> {
        self.mm_snapshot().ok_or(UserAccessError::Fault)?.zero_user(
            user_address,
            length,
            self.user_fault_limits(),
        )
    }

    pub(crate) fn validate_user_write(
        &self,
        user_address: usize,
        length: usize,
    ) -> Result<(), UserAccessError> {
        self.mm_snapshot()
            .ok_or(UserAccessError::Fault)?
            .validate_user_write(user_address, length, self.user_fault_limits())
    }

    /// proc cmdline 从此实际 Thread 的 MM 读取；leader 退出不能让 sibling 借已退休 leader 的 MM。
    pub(in crate::execution::task) fn proc_arguments(
        &self,
    ) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.mm_snapshot()
            .map_or_else(|| Ok(alloc::vec::Vec::new()), |mm| mm.process_arguments())
    }

    pub(in crate::execution::task) fn write_clone_tid_values(
        &self,
        addresses: [Option<usize>; 2],
        tid: i32,
    ) {
        self.address_space()
            .write_clone_tid_values(addresses, tid, self.user_fault_limits())
    }

    pub(crate) fn copy_user_c_string(
        &self,
        user_address: usize,
        max_len: usize,
    ) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.mm_snapshot()
            .ok_or(UserAccessError::Fault)?
            .copy_user_c_string(user_address, max_len, self.user_fault_limits())
    }

    /// 返回当前 AddressSpace 生命周期内不变的 arch token。
    pub(crate) fn user_token(&self) -> crate::arch::mmu::AddressSpaceToken {
        self.address_space().token.clone()
    }

    pub(crate) fn with_futex_key<R>(
        &self,
        address: usize,
        private: bool,
        consume: impl FnOnce(FutexKey) -> R,
    ) -> Result<R, UserAccessError> {
        self.address_space()
            .with_futex_key(address, private, self.user_fault_limits(), consume)
    }

    pub(crate) fn with_futex_word<R>(
        &self,
        address: usize,
        private: bool,
        consume: impl FnOnce(FutexKey, u32) -> R,
    ) -> Result<R, UserAccessError> {
        self.address_space()
            .with_futex_word(address, private, self.user_fault_limits(), consume)
    }

    pub(crate) fn with_futex_requeue<R>(
        &self,
        source: usize,
        target: usize,
        private: bool,
        read_source: bool,
        consume: impl FnOnce(FutexKey, FutexKey, Option<u32>) -> R,
    ) -> Result<R, crate::memory::FutexAccessError> {
        self.address_space().with_futex_requeue(
            source,
            target,
            private,
            self.user_fault_limits(),
            read_source,
            consume,
        )
    }

    /// task futex owner 的 scoped MM transaction；不向 syscall 暴露 MemorySet guard。
    pub(in crate::execution::task) fn with_futex_wake_op<R>(
        &self,
        source: usize,
        target: usize,
        private: bool,
        consume: impl FnOnce(FutexKey, FutexKey, &mut MemorySet) -> R,
    ) -> Result<R, crate::memory::FutexAccessError> {
        self.address_space().with_futex_wake_op(
            source,
            target,
            private,
            self.user_fault_limits(),
            consume,
        )
    }
}

// Process 生命周期内唯一投影；graph/proc observer 不保活任何 TCB。
impl super::Process {
    pub(in crate::execution::task) fn process_statistics(
        &self,
        threads: &[Arc<TaskControlBlock>],
    ) -> Result<ProcessStatistics, ()> {
        let leader_mm = self.process_address_space();
        let has_mm = leader_mm.is_some();
        // THREAD 必须共享 VM；group common projection 只扫描一次，leader None 时 worker 仍可有 mm。
        let mm = leader_mm.or_else(|| threads.iter().find_map(|thread| thread.mm_snapshot()));
        let pages = match mm {
            Some(mm) => mm.page_statistics().map_err(|_| ())?,
            None => crate::memory::UserPageStatistics::default(),
        };
        let comm_snapshot = self.comm_snapshot().map_err(|_| ())?;
        Ok(ProcessStatistics {
            comm: comm_snapshot,
            has_mm,
            start_time_us: self.start_time_us,
            virtual_pages: pages.virtual_pages,
            resident_pages: pages.resident_pages,
            shared_pages: pages.shared_pages,
            text_pages: pages.text_pages,
            data_pages: pages.data_pages,
            swapped_pages: pages.swapped_pages,
            fd_size: self
                .with_process_files(|files| files.slot_capacity())
                .unwrap_or(0),
        })
    }
}

impl TaskControlBlock {
    /// 目标已授权的 debugger word 访问，MM 锁和 canonical fault/COW 全由原 owner 执行。
    ///
    /// # Parameters
    /// - `address`: 目标虚拟地址。
    /// - `bytes`: 八字节 kernel staging。
    /// - `write`: true 写入、false 读取。
    ///
    /// # Returns
    /// 完整复制成功。
    ///
    /// # Errors
    /// MM 退休、waiter OOM、地址或 backing 失败。
    pub(in crate::execution::task) fn ptrace_word(
        &self,
        address: usize,
        bytes: &mut [u8; 8],
        write: bool,
    ) -> Result<(), UserAccessError> {
        let mm = self.mm_snapshot().ok_or(UserAccessError::Fault)?;
        mm.memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?
            .ptrace_word(address, bytes, write, self.user_fault_limits())
    }
}
