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

impl SharedMappingInvalidator for AddressSpace {
    fn invalidate_shared_file(&self, id: SharedFileId, size: u64) {
        self.memory_set.lock().invalidate_shared_file(id, size);
    }
}
