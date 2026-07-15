use super::*;

impl TaskControlBlock {
    pub(crate) fn set_program_break(&self, new_break: usize) -> Result<usize, MemoryError> {
        let address_space_limit = self
            .resource_limit(RLIMIT_AS)
            .expect("RLIMIT_AS must exist")
            .soft;
        let data_limit = self
            .resource_limit(RLIMIT_DATA)
            .expect("RLIMIT_DATA must exist")
            .soft;
        self.process
            .address_space()
            .memory_set
            .lock()
            .set_program_break(new_break, address_space_limit, data_limit)
    }

    pub(crate) fn map_anonymous(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
    ) -> Result<usize, MemoryError> {
        let address_space_limit = self.resource_limit(RLIMIT_AS).unwrap().soft;
        let data_limit = self.resource_limit(RLIMIT_DATA).unwrap().soft;
        self.process.address_space().map_anonymous(
            address,
            length,
            permission,
            fixed_noreplace,
            address_space_limit,
            data_limit,
        )
    }

    pub(crate) fn map_private_file(
        &self,
        address: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        source: FileMappingSource,
    ) -> Result<usize, MemoryError> {
        let address_space_limit = self.resource_limit(RLIMIT_AS).unwrap().soft;
        let data_limit = self.resource_limit(RLIMIT_DATA).unwrap().soft;
        self.process.address_space().map_private_file(
            address,
            permission,
            fixed_noreplace,
            source,
            MappingResourceLimits::new(address_space_limit, data_limit),
        )
    }

    pub(crate) fn map_shared_file(
        &self,
        address: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        source: FileMappingSource,
    ) -> Result<usize, MemoryError> {
        let address_space_limit = self.resource_limit(RLIMIT_AS).unwrap().soft;
        self.process.address_space().map_shared_file(
            address,
            permission,
            fixed_noreplace,
            source,
            address_space_limit,
        )
    }

    /// @description 将 DRM 已授权的 device backing 映射进 calling Process AddressSpace。
    ///
    /// @param address 零为内核选址，非零为 hint 或 exact address。
    /// @param length 映射字节长度。
    /// @param permission 用户 read/write/none 权限。
    /// @param fixed_noreplace 是否禁止覆盖已有 VMA。
    /// @param source 已验证 handle 与 mmap offset 的 backing owner。
    /// @return 成功返回 mapping 起点；范围、权限或内存错误保持 transaction 未发布。
    pub(crate) fn map_device(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        source: DeviceMappingSource,
    ) -> Result<usize, MemoryError> {
        let address_space_limit = self.resource_limit(RLIMIT_AS).unwrap().soft;
        self.process.address_space().map_device(
            address,
            length,
            permission,
            fixed_noreplace,
            source,
            address_space_limit,
        )
    }

    pub(crate) fn sync_shared_mapping(
        &self,
        address: usize,
        length: usize,
        writeback: bool,
    ) -> Result<(), MemoryError> {
        self.process
            .address_space()
            .sync_shared_mapping(address, length, writeback)
    }

    pub(crate) fn handle_page_fault(
        &self,
        address: usize,
        access: PageFaultAccess,
    ) -> Result<PageFaultOutcome, MemoryError> {
        self.process
            .address_space()
            .handle_page_fault(address, access, self.user_fault_limits())
    }

    pub(crate) fn unmap_user_mapping(
        &self,
        address: usize,
        length: usize,
    ) -> Result<(), MemoryError> {
        self.process
            .address_space()
            .unmap_user_mapping(address, length)
    }

    pub(crate) fn protect_user_mapping(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        self.process
            .address_space()
            .protect_user_mapping(address, length, permission)
    }

    pub(crate) fn advise_user_mapping(
        &self,
        address: usize,
        length: usize,
        advice: crate::memory::MemoryAdvice,
    ) -> Result<(), MemoryError> {
        self.process
            .address_space()
            .advise_user_mapping(address, length, advice)
    }

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
        let address_space_limit = self.resource_limit(RLIMIT_AS).unwrap().soft;
        self.process.address_space().map_shared_anonymous(
            address,
            length,
            permission,
            fixed_noreplace,
            address_space_limit,
        )
    }
}

impl AddressSpace {
    fn map_device(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        source: DeviceMappingSource,
        address_space_limit: u64,
    ) -> Result<usize, MemoryError> {
        self.memory_set.lock().map_device(
            address,
            length,
            permission,
            fixed_noreplace,
            source,
            address_space_limit,
        )
    }
}
