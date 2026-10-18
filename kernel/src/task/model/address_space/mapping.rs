use super::*;

impl TaskControlBlock {
    /// `mremap`：一次 MM 事务完成缩小、增长或搬迁，fence 在释放 MM lock 后同步。
    pub(crate) fn remap_user_mapping(
        &self,
        request: crate::memory::RemapRequest,
    ) -> Result<usize, crate::memory::RemapError> {
        self.address_space()
            .remap_user_mapping(request, self.mapping_resource_limits())
    }

    pub(crate) fn set_program_break(&self, new_break: usize) -> Result<usize, MemoryError> {
        let address_space_limit = self
            .resource_limit(RLIMIT_AS)
            .expect("RLIMIT_AS must exist")
            .soft;
        let data_limit = self
            .resource_limit(RLIMIT_DATA)
            .expect("RLIMIT_DATA must exist")
            .soft;
        self.address_space()
            .set_program_break(new_break, address_space_limit, data_limit)
    }

    /// mmap 与 shmat 共用完整 MM transaction；raw flags 与 backing 授权由上层完成。
    pub(crate) fn install_user_mapping(
        &self,
        request: crate::memory::UserMappingRequest,
    ) -> Result<usize, MemoryError> {
        self.address_space()
            .install_user_mapping(request, self.mapping_resource_limits())
    }

    pub(crate) fn sync_shared_mapping(
        &self,
        address: usize,
        length: usize,
        writeback: bool,
    ) -> Result<(), MemoryError> {
        self.address_space()
            .sync_shared_mapping(address, length, writeback)
    }

    pub(crate) fn handle_page_fault(
        &self,
        address: usize,
        access: PageFaultAccess,
    ) -> Result<PageFaultOutcome, MemoryError> {
        self.address_space()
            .handle_page_fault(address, access, self.user_fault_limits())
    }

    pub(crate) fn unmap_user_mapping(
        &self,
        address: usize,
        length: usize,
    ) -> Result<(), MemoryError> {
        self.address_space().unmap_user_mapping(address, length)
    }

    pub(crate) fn protect_user_mapping(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        self.address_space()
            .protect_user_mapping(address, length, permission)
    }

    pub(crate) fn advise_user_mapping(
        &self,
        address: usize,
        length: usize,
        advice: crate::memory::MemoryAdvice,
    ) -> Result<(), MemoryError> {
        self.address_space()
            .advise_user_mapping(address, length, advice)
    }
}

impl AddressSpace {
    fn remap_user_mapping(
        &self,
        request: crate::memory::RemapRequest,
        limits: MappingResourceLimits,
    ) -> Result<usize, crate::memory::RemapError> {
        let prepared = {
            let mut memory = self
                .memory_set
                .lock()
                .map_err(|_| crate::memory::RemapError::Memory(MemoryError::OutOfMemory))?;
            memory.prepare_user_remap(request, limits)
        };
        prepared.synchronize()
    }

    fn set_program_break(
        &self,
        new_break: usize,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<usize, MemoryError> {
        let (current, retirement) = {
            let mut memory = self
                .memory_set
                .lock()
                .map_err(|_| MemoryError::OutOfMemory)?;
            memory.set_program_break(new_break, address_space_limit, data_limit)?
        };
        if let Some(retirement) = retirement {
            retirement.synchronize();
        }
        Ok(current)
    }

    fn install_user_mapping(
        &self,
        request: crate::memory::UserMappingRequest,
        limits: MappingResourceLimits,
    ) -> Result<usize, MemoryError> {
        // pin 实际 OFD/device/shm preparation，所有错误的最后析构均晚于 MM guard。
        let retained = request.source.clone();
        let prepared = {
            let mut memory = self
                .memory_set
                .lock()
                .map_err(|_| MemoryError::OutOfMemory)?;
            memory.prepare_user_mapping(request, limits)
        };
        let result = prepared.and_then(|prepared| prepared.synchronize());
        drop(retained);
        result
    }
}
