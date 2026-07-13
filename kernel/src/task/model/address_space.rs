use super::*;

#[derive(Debug)]
pub(super) struct AddressSpace {
    pub(super) memory_set: Mutex<MemorySet>,
}

impl AddressSpace {
    pub(super) fn new(memory_set: MemorySet) -> Result<Arc<Self>, MemoryError> {
        let owner = Arc::new(Self {
            memory_set: Mutex::new(memory_set),
        });
        crate::memory::register_shared_mapping_owner(owner.clone())
            .map_err(|_| MemoryError::OutOfMemory)?;
        Ok(owner)
    }

    pub(super) fn page_statistics(&self) -> (usize, usize) {
        self.memory_set.lock().user_page_statistics()
    }

    /// @description 按 Linux mm argument range 复制当前 Process 的实时 argv bytes。
    /// @return range 可读时返回 NUL 分隔 bytes；unmap/protection/resource 失败返回 None。
    pub(super) fn process_arguments(&self) -> Option<alloc::vec::Vec<u8>> {
        self.memory_set.lock().process_arguments().ok()
    }
    pub(super) fn write_clone_tid_values(
        &self,
        addresses: [Option<usize>; 2],
        tid: i32,
    ) -> Result<(), UserAccessError> {
        let mut memory = self.memory_set.lock();
        for address in addresses.into_iter().flatten() {
            memory.validate_user_write(address, core::mem::size_of::<i32>())?;
        }
        for address in addresses.into_iter().flatten() {
            memory.copy_to_user(address, &tid.to_ne_bytes())?;
        }
        Ok(())
    }

    pub(super) fn map_anonymous(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
    ) -> Result<usize, MemoryError> {
        self.memory_set
            .lock()
            .map_anonymous(address, length, permission, fixed_noreplace)
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
    ) -> Result<usize, MemoryError> {
        self.memory_set
            .lock()
            .map_shared_anonymous(address, length, permission, fixed_noreplace)
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
        consume: impl FnOnce(FutexKey) -> R,
    ) -> Result<R, UserAccessError> {
        let identity = self as *const Self as usize;
        let mut memory = self.memory_set.lock();
        let key = memory.futex_key(address, identity, private)?;
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
        consume: impl FnOnce(FutexKey, u32) -> R,
    ) -> Result<R, UserAccessError> {
        let identity = self as *const Self as usize;
        let mut memory = self.memory_set.lock();
        let key = memory.futex_key(address, identity, private)?;
        let mut bytes = [0u8; core::mem::size_of::<u32>()];
        memory.copy_from_user(address, &mut bytes)?;
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
        consume: impl FnOnce(FutexKey, FutexKey, u32) -> R,
    ) -> Result<R, UserAccessError> {
        let identity = self as *const Self as usize;
        let mut memory = self.memory_set.lock();
        let source_key = memory.futex_key(source, identity, private)?;
        let target_key = memory.futex_key(target, identity, private)?;
        let mut bytes = [0u8; core::mem::size_of::<u32>()];
        memory.copy_from_user(source, &mut bytes)?;
        Ok(consume(source_key, target_key, u32::from_ne_bytes(bytes)))
    }

    pub(super) fn map_private_file(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        data: &[u8],
    ) -> Result<usize, MemoryError> {
        self.memory_set
            .lock()
            .map_private_file(address, length, permission, fixed_noreplace, data)
    }

    pub(super) fn map_shared_file(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        mapping: Arc<dyn SharedFileMapping>,
        offset: u64,
    ) -> Result<usize, MemoryError> {
        self.memory_set.lock().map_shared_file(
            address,
            length,
            permission,
            fixed_noreplace,
            mapping,
            offset,
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
    ) -> Result<PageFaultOutcome, MemoryError> {
        self.memory_set.lock().handle_page_fault(address, access)
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

    /// @description 从用户地址空间复制字节到 kernel 缓冲区，地址空间锁覆盖整个复制。
    ///
    /// @param user_address 用户源地址。
    /// @param destination kernel 目标缓冲区。
    /// @return 完整成功返回 `Ok(())`；fault、权限错误或 overflow 返回 `UserAccessError`。
    pub(super) fn copy_from_user(
        &self,
        user_address: usize,
        destination: &mut [u8],
    ) -> Result<(), UserAccessError> {
        self.memory_set
            .lock()
            .copy_from_user(user_address, destination)
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
    ) -> Result<(), UserAccessError> {
        self.memory_set.lock().copy_to_user(user_address, source)
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
    ) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.memory_set
            .lock()
            .copy_user_c_string(user_address, max_len)
    }
}

impl TaskControlBlock {
    /// @description 通过 calling Process 的唯一 AddressSpace owner 建立 anonymous shared mapping。
    ///
    /// @param address 零为内核选址，非零为 hint 或 fixed_noreplace exact address。
    /// @param length 非零 mapping 字节长度。
    /// @param permission 用户页权限。
    /// @param fixed_noreplace 是否禁止覆盖已有 VMA。
    /// @return 成功返回 mapping 起点；非法范围、冲突或内存不足返回 MemoryError。
    pub(crate) fn map_shared_anonymous(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
    ) -> Result<usize, MemoryError> {
        self.process.address_space.map_shared_anonymous(
            address,
            length,
            permission,
            fixed_noreplace,
        )
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
        self.process
            .address_space
            .with_futex_key(address, private, consume)
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
        self.process
            .address_space
            .with_futex_word(address, private, consume)
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
        self.process
            .address_space
            .with_futex_requeue(source, target, private, consume)
    }

    /// @description 通过 Process address-space owner 读取实时 argv bytes。
    /// @return argument range 可读时返回 NUL 分隔 bytes；否则返回 None。
    pub(in crate::task) fn process_arguments(&self) -> Option<alloc::vec::Vec<u8>> {
        self.process.address_space.process_arguments()
    }

    pub(in crate::task) fn process_statistics(&self) -> (Vec<u8>, u64, usize, usize, usize) {
        let (virtual_pages, resident_pages) = self.process.address_space.page_statistics();
        (
            self.process.comm.lock().clone(),
            self.process.start_time_us,
            virtual_pages,
            resident_pages,
            self.process.files.lock().slot_capacity(),
        )
    }
}

impl SharedMappingInvalidator for AddressSpace {
    fn invalidate_shared_file(&self, id: SharedFileId, size: u64) {
        self.memory_set.lock().invalidate_shared_file(id, size);
    }
}
