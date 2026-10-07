//! ext4 的 `mount(2)` 类型适配。独立于 `ext4` 模块：后者被 host 单元测试直接包含，不能依赖设备注册表。

use alloc::sync::Arc;

use super::{
    FileSystem, FileSystemError, device,
    ext4::Ext4FileSystem,
    mount::{FileSystemType, MountRequest},
    mount_options,
};

/// ext4 类型：固定 profile，不接受选项。
pub(super) struct Ext4FileSystemType;

impl FileSystemType for Ext4FileSystemType {
    fn name(&self) -> &'static str {
        "ext4"
    }

    fn requires_device(&self) -> bool {
        true
    }

    fn create(&self, request: &MountRequest<'_>) -> Result<Arc<dyn FileSystem>, FileSystemError> {
        if !mount_options::is_empty(request.options) {
            return Err(FileSystemError::InvalidOperation);
        }
        let number = request.device.ok_or(FileSystemError::InvalidOperation)?;
        let disk = device::block_device(number).ok_or(FileSystemError::NoDevice)?;
        let filesystem = Ext4FileSystem::new(disk)?;
        filesystem.start_writeback(request.environment.threads)?;
        Ok(filesystem)
    }
}
