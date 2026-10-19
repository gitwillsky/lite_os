//! ext4 的 `mount(2)` 类型适配。独立于 `ext4` 模块：后者被 host 单元测试直接包含，不能依赖设备注册表。

use super::{
    FileSystemError, devnode,
    ext4::Ext4FileSystem,
    mount::{CreatedFileSystem, FileSystemType, MountRequest},
};

/// ext4 类型：固定 ordered profile，允许显式重述实际 data 模式。
pub(super) struct Ext4FileSystemType;

impl FileSystemType for Ext4FileSystemType {
    fn name(&self) -> &'static str {
        "ext4"
    }

    fn requires_device(&self) -> bool {
        true
    }

    fn validate_mount_options(&self, options: &[u8]) -> Result<(), FileSystemError> {
        super::ext4::validate_mount_options(options)
    }

    fn create(&self, request: &MountRequest<'_>) -> Result<CreatedFileSystem, FileSystemError> {
        self.validate_mount_options(request.options)?;
        let number = request.device.ok_or(FileSystemError::InvalidOperation)?;
        let disk = devnode::block_device(number).ok_or(FileSystemError::NoDevice)?;
        let filesystem = Ext4FileSystem::new(disk, number, request.read_only)?;
        filesystem.start_writeback(request.environment.threads)?;
        Ok(CreatedFileSystem {
            filesystem,
            user_namespace: request.identity.user_namespace(),
        })
    }
}
