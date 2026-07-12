use alloc::{sync::Arc, vec, vec::Vec};
use spin::Once;

use super::{
    DeviceKind, DirectoryEntry, FileSystem, FileSystemError, FileSystemStatistics, Inode,
    InodeMetadata, InodeType,
};

const DEVICE_FILESYSTEM_ID: usize = 2;

// OWNER: devfs module 唯一拥有 synthetic device filesystem；缺失会产生重复 st_dev/inode identity。
static DEVICE_FILESYSTEM: Once<Arc<DevFileSystem>> = Once::new();

#[derive(Clone, Copy)]
enum DevNode {
    Root,
    Device(DeviceKind),
}

impl DevNode {
    fn inode(self) -> u64 {
        match self {
            Self::Root => 1,
            Self::Device(device) => device.inode(),
        }
    }

    fn mode(self) -> u32 {
        match self {
            Self::Root => 0o040755,
            Self::Device(device) => device.mode(),
        }
    }
}

struct DevInode {
    filesystem_id: usize,
    node: DevNode,
}

impl DevInode {
    fn new(filesystem_id: usize, node: DevNode) -> Arc<Self> {
        Arc::new(Self {
            filesystem_id,
            node,
        })
    }

    fn child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !matches!(self.node, DevNode::Root) {
            return Err(FileSystemError::NotDirectory);
        }
        let node = match name {
            b"." | b".." => DevNode::Root,
            b"null" => DevNode::Device(DeviceKind::Null),
            b"zero" => DevNode::Device(DeviceKind::Zero),
            b"tty" => DevNode::Device(DeviceKind::Tty),
            b"console" => DevNode::Device(DeviceKind::Console),
            _ => return Err(FileSystemError::NotFound),
        };
        Ok(Self::new(self.filesystem_id, node))
    }
}

impl Inode for DevInode {
    fn filesystem_id(&self) -> usize {
        self.filesystem_id
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let device = match self.node {
            DevNode::Root => None,
            DevNode::Device(device) => Some(device),
        };
        Ok(InodeMetadata {
            filesystem: self.filesystem_id as u64,
            inode: self.node.inode(),
            kind: self.inode_type(),
            mode: self.node.mode(),
            links: if device.is_some() { 1 } else { 2 },
            uid: 0,
            gid: 0,
            size: 0,
            blocks: 0,
            block_size: 4096,
            atime: 0,
            mtime: 0,
            ctime: 0,
            device,
        })
    }

    fn inode_type(&self) -> InodeType {
        match self.node {
            DevNode::Root => InodeType::Directory,
            DevNode::Device(_) => InodeType::CharacterDevice,
        }
    }

    fn size(&self) -> u64 {
        0
    }

    fn is_executable(&self) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn device_kind(&self) -> Option<DeviceKind> {
        match self.node {
            DevNode::Root => None,
            DevNode::Device(device) => Some(device),
        }
    }

    fn read_storage(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn write_storage(&self, _offset: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn append_storage(&self, _buf: &[u8]) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn truncate_storage(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    fn list(&self) -> Result<Vec<DirectoryEntry>, FileSystemError> {
        if !matches!(self.node, DevNode::Root) {
            return Err(FileSystemError::NotDirectory);
        }
        Ok(vec![
            DirectoryEntry {
                inode: 1,
                kind: InodeType::Directory,
                name: b".".to_vec(),
            },
            DirectoryEntry {
                inode: 1,
                kind: InodeType::Directory,
                name: b"..".to_vec(),
            },
            DirectoryEntry {
                inode: 2,
                kind: InodeType::CharacterDevice,
                name: b"null".to_vec(),
            },
            DirectoryEntry {
                inode: 3,
                kind: InodeType::CharacterDevice,
                name: b"zero".to_vec(),
            },
            DirectoryEntry {
                inode: 4,
                kind: InodeType::CharacterDevice,
                name: b"tty".to_vec(),
            },
            DirectoryEntry {
                inode: 5,
                kind: InodeType::CharacterDevice,
                name: b"console".to_vec(),
            },
        ])
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.child(name)
    }

    fn create(
        &self,
        _name: &[u8],
        _kind: InodeType,
        _metadata: super::CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn unlink(&self, _name: &[u8], _remove_directory: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn rename(
        &self,
        _old_name: &[u8],
        _new_parent_inode: u64,
        _new_name: &[u8],
        _no_replace: bool,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
}

/// @description 固定设备集合的只读 devfs adapter。
pub(crate) struct DevFileSystem {
    root: Arc<DevInode>,
}

impl DevFileSystem {
    /// @description 取得包含 null/zero/tty/console 的唯一 device filesystem。
    pub(crate) fn instance() -> Arc<Self> {
        DEVICE_FILESYSTEM
            .call_once(|| {
                Arc::new(Self {
                    root: DevInode::new(DEVICE_FILESYSTEM_ID, DevNode::Root),
                })
            })
            .clone()
    }
}

impl FileSystem for DevFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    fn statistics(&self) -> FileSystemStatistics {
        FileSystemStatistics {
            type_name: "devfs",
            magic: 0x8584_58f6,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: [DEVICE_FILESYSTEM_ID as u32, 0],
            name_length: 255,
            fragment_size: 4096,
            flags: 1,
        }
    }
}
