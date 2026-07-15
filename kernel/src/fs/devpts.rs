use alloc::{sync::Arc, vec::Vec};

use super::{
    DeviceKind, DirectoryEntry, FileSystem, FileSystemError, FileSystemStatistics, Inode,
    InodeMetadata, InodeType,
};

const DEVPTS_FILESYSTEM_ID: usize = 5;
const DEVPTS_SUPER_MAGIC: u64 = 0x1cd1;

#[derive(Clone, Copy)]
enum DevPtsNode {
    Root,
    Slave(u32),
}

impl DevPtsNode {
    fn inode(self) -> u64 {
        match self {
            Self::Root => 1,
            Self::Slave(index) => 3 + u64::from(index),
        }
    }
}

struct DevPtsInode {
    node: DevPtsNode,
}

impl DevPtsInode {
    fn new(node: DevPtsNode) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self { node }).map_err(|_| FileSystemError::OutOfMemory)
    }

    fn child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        let node = match (self.node, name) {
            (DevPtsNode::Root, b"." | b"..") => DevPtsNode::Root,
            (DevPtsNode::Root, name) => {
                let index = parse_index(name).ok_or(FileSystemError::NotFound)?;
                if !super::pty::slave_exists(index) {
                    return Err(FileSystemError::NotFound);
                }
                DevPtsNode::Slave(index)
            }
            (DevPtsNode::Slave(_), _) => return Err(FileSystemError::NotDirectory),
        };
        Ok(Self::new(node)?)
    }
}

fn parse_index(name: &[u8]) -> Option<u32> {
    if name.is_empty() || name.len() > 10 {
        return None;
    }
    name.iter().try_fold(0u32, |value, byte| {
        let digit = byte.checked_sub(b'0')?;
        (digit <= 9)
            .then(|| value.checked_mul(10)?.checked_add(u32::from(digit)))
            .flatten()
    })
}

fn index_name(index: u32, output: &mut [u8; 10]) -> &[u8] {
    let mut reverse = [0u8; 10];
    let mut value = index;
    let mut length = 0;
    loop {
        reverse[length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for offset in 0..length {
        output[offset] = reverse[length - offset - 1];
    }
    &output[..length]
}

impl Inode for DevPtsInode {
    fn filesystem_id(&self) -> usize {
        DEVPTS_FILESYSTEM_ID
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let (kind, mode, device) = match self.node {
            DevPtsNode::Root => (InodeType::Directory, 0o040755, None),
            DevPtsNode::Slave(index) => (
                InodeType::CharacterDevice,
                DeviceKind::PtySlave(index).mode(),
                Some(DeviceKind::PtySlave(index)),
            ),
        };
        Ok(InodeMetadata {
            filesystem: DEVPTS_FILESYSTEM_ID as u64,
            inode: self.node.inode(),
            kind,
            mode,
            links: if matches!(self.node, DevPtsNode::Root) {
                2
            } else {
                1
            },
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
            DevPtsNode::Root => InodeType::Directory,
            DevPtsNode::Slave(_) => InodeType::CharacterDevice,
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
            DevPtsNode::Root => None,
            DevPtsNode::Slave(index) => Some(DeviceKind::PtySlave(index)),
        }
    }

    fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
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
        if !matches!(self.node, DevPtsNode::Root) {
            return Err(FileSystemError::NotDirectory);
        }
        let indices = super::pty::slave_indices()?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(indices.len().saturating_add(2))
            .map_err(|_| FileSystemError::OutOfMemory)?;
        entries.push(DirectoryEntry::try_new(1, InodeType::Directory, b".")?);
        entries.push(DirectoryEntry::try_new(1, InodeType::Directory, b"..")?);
        for index in indices {
            let mut storage = [0u8; 10];
            entries.push(DirectoryEntry::try_new(
                DevPtsNode::Slave(index).inode(),
                InodeType::CharacterDevice,
                index_name(index, &mut storage),
            )?);
        }
        Ok(entries)
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

/// @description Unix98 PTY slave namespace filesystem；节点生命周期由 pty registry 投影。
pub(crate) struct DevPtsFileSystem {
    root: Arc<DevPtsInode>,
}

impl DevPtsFileSystem {
    /// @description 构造挂载到 `/dev/pts` 的独立 devpts instance。
    /// @return 新 filesystem；root 或 filesystem Arc OOM 返回错误。
    pub(crate) fn new() -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            root: DevPtsInode::new(DevPtsNode::Root)?,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
}

impl FileSystem for DevPtsFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    fn statistics(&self) -> FileSystemStatistics {
        FileSystemStatistics {
            type_name: "devpts",
            magic: DEVPTS_SUPER_MAGIC,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: [DEVPTS_FILESYSTEM_ID as u32, 0],
            name_length: 255,
            fragment_size: 4096,
            flags: 0,
        }
    }
}
