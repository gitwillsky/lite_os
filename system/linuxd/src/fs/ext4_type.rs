//! ext4 的 `mount(2)` 类型：文件系统由 ext4d 服务承载，本 VFS 经块设备型 FUSE 挂载访问它。
//!
//! 1. 校验选项后把源块设备的整个区间共享给 ext4d，ext4d 打开文件系统（回放日志）并交回 FUSE channel
//!    与挂载控制；
//! 2. FUSE 挂载承担 inode、页缓存与权限检查；本类型只投影 ext4 自己的 superblock 事实：类型名、
//!    `data=ordered` 选项、`EXT4_SUPER_MAGIC` 与由 UUID 折叠的 fsid；
//! 3. 只读挂载的 ro → rw 重新挂载经挂载控制让 ext4d 完成推迟的写前准备。

use alloc::{sync::Arc, vec::Vec};

use native_runtime::native_abi::{self, Status};

use super::{
    FileSystem, FileSystemError, FileSystemStatistics, Inode, SuperblockFacts, devnode,
    ext4_identity::ext4_identity,
    mount::{CreatedFileSystem, FileSystemType, MountRequest},
    mount_options,
};
use filesystem_protocol::RemoteMount;

/// Linux `EXT4_SUPER_MAGIC`。
const EXT4_SUPER_MAGIC: u64 = 0xEF53;
/// 超级块在设备上的字节偏移与长度。
const SUPERBLOCK_OFFSET: u64 = 1024;
const SUPERBLOCK_SIZE: usize = 1024;

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
        validate_mount_options(options)
    }

    fn create(&self, request: &MountRequest<'_>) -> Result<CreatedFileSystem, FileSystemError> {
        self.validate_mount_options(request.options)?;
        let number = request.device.ok_or(FileSystemError::InvalidOperation)?;
        let node = devnode::block_node(number).ok_or(FileSystemError::NoDevice)?;
        let mut superblock = [0u8; SUPERBLOCK_SIZE];
        if node.read(SUPERBLOCK_OFFSET, &mut superblock)? != SUPERBLOCK_SIZE {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let (uuid, _) = ext4_identity(&superblock).ok_or(FileSystemError::InvalidOperation)?;
        let disk = node.device();
        let block = disk
            .share_range(0, disk.block_count())
            .ok_or(FileSystemError::NoDevice)?;
        let (channel, control) = crate::handles::ext4()
            .mount(block, request.read_only)
            .map_err(status_error)?;
        let inner = super::fuse::mount_service(channel, request.environment)?;
        let filesystem = Arc::try_new(Ext4Mount {
            inner,
            control,
            fsid: fsid(&uuid),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        Ok(CreatedFileSystem {
            filesystem,
            user_namespace: request.identity.user_namespace(),
        })
    }
}

/// 首次挂载与 remount 共用固定 ordered profile；未知 key 或其他 data 模式返回 EINVAL。
fn validate_mount_options(options: &[u8]) -> Result<(), FileSystemError> {
    if mount_options::parse(options)
        .all(|option| option.key == b"data" && option.value == Some(b"ordered".as_slice()))
    {
        Ok(())
    } else {
        Err(FileSystemError::InvalidOperation)
    }
}

/// Linux `uuid_to_fsid`：两个 little-endian 64 位半段异或后折叠为两个 32 位字。
fn fsid(uuid: &[u8; 16]) -> [u32; 2] {
    let mut halves = [[0u8; 8]; 2];
    halves[0].copy_from_slice(&uuid[..8]);
    halves[1].copy_from_slice(&uuid[8..]);
    let folded = u64::from_le_bytes(halves[0]) ^ u64::from_le_bytes(halves[1]);
    [folded as u32, (folded >> 32) as u32]
}

/// ext4d 报告的挂载失败：无效文件系统按 Linux `mount(2)` 报 `EINVAL`。
fn status_error(status: Status) -> FileSystemError {
    match status {
        native_abi::ERR_NO_MEMORY => FileSystemError::OutOfMemory,
        native_abi::ERR_INVALID_ARGS => FileSystemError::InvalidOperation,
        _ => FileSystemError::IoError,
    }
}

/// 一个 ext4 挂载：FUSE 挂载加上 ext4 的 superblock 事实与挂载控制。
struct Ext4Mount {
    inner: Arc<dyn FileSystem>,
    control: RemoteMount,
    fsid: [u32; 2],
}

impl FileSystem for Ext4Mount {
    fn show_mount_options(&self) -> Result<Vec<u8>, FileSystemError> {
        let mut options = Vec::new();
        options
            .try_reserve_exact(b",data=ordered".len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        options.extend_from_slice(b",data=ordered");
        Ok(options)
    }

    fn superblock(&self) -> SuperblockFacts {
        SuperblockFacts {
            type_name: "ext4",
            read_only: false,
        }
    }

    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.inner.root_inode()
    }

    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        let mut statistics = self.inner.statistics()?;
        statistics.magic = EXT4_SUPER_MAGIC;
        statistics.fsid = self.fsid;
        Ok(statistics)
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        self.inner.sync()
    }

    fn shutdown(&self) -> Result<(), FileSystemError> {
        self.inner.shutdown()
    }

    fn remount(
        &self,
        options: &[u8],
        _namespace: &crate::namespace::UserNamespace,
        writable_transition: bool,
    ) -> Result<(), FileSystemError> {
        validate_mount_options(options)?;
        if writable_transition {
            self.control.make_writable().map_err(status_error)?;
        }
        Ok(())
    }
}
