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
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        mapping: Arc<dyn SharedFileMapping>,
        offset: u64,
    ) -> Result<usize, MemoryError> {
        let address_space_limit = self.resource_limit(RLIMIT_AS).unwrap().soft;
        let data_limit = self.resource_limit(RLIMIT_DATA).unwrap().soft;
        self.process.address_space().map_private_file(
            address,
            length,
            permission,
            fixed_noreplace,
            FileMappingSource::new(mapping, offset),
            MappingResourceLimits::new(address_space_limit, data_limit),
        )
    }

    pub(crate) fn map_shared_file(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        mapping: Arc<dyn SharedFileMapping>,
        offset: u64,
    ) -> Result<usize, MemoryError> {
        let address_space_limit = self.resource_limit(RLIMIT_AS).unwrap().soft;
        self.process.address_space().map_shared_file(
            address,
            length,
            permission,
            fixed_noreplace,
            FileMappingSource::new(mapping, offset),
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
