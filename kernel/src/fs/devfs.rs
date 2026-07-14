use alloc::{sync::Arc, vec::Vec};
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
    Dri,
    Device(DeviceKind),
    Link(DevLink),
}

#[derive(Clone, Copy)]
enum DevLink {
    Fd,
    Stdin,
    Stdout,
    Stderr,
}

impl DevLink {
    fn target(self) -> &'static [u8] {
        match self {
            Self::Fd => b"/proc/self/fd",
            Self::Stdin => b"/proc/self/fd/0",
            Self::Stdout => b"/proc/self/fd/1",
            Self::Stderr => b"/proc/self/fd/2",
        }
    }
}

impl DevNode {
    fn inode(self) -> u64 {
        match self {
            Self::Root => 1,
            Self::Dri => 12,
            Self::Device(device) => device.inode(),
            Self::Link(DevLink::Fd) => 6,
            Self::Link(DevLink::Stdin) => 7,
            Self::Link(DevLink::Stdout) => 8,
            Self::Link(DevLink::Stderr) => 9,
        }
    }

    fn mode(self) -> u32 {
        match self {
            Self::Root | Self::Dri => 0o040755,
            Self::Device(device) => device.mode(),
            Self::Link(_) => 0o120777,
        }
    }
}

struct DevInode {
    filesystem_id: usize,
    node: DevNode,
}

impl DevInode {
    fn new(filesystem_id: usize, node: DevNode) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            filesystem_id,
            node,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    fn child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        let node = match (self.node, name) {
            (DevNode::Root, b"." | b"..") => DevNode::Root,
            (DevNode::Root, b"dri") => DevNode::Dri,
            (DevNode::Root, b"null") => DevNode::Device(DeviceKind::Null),
            (DevNode::Root, b"zero") => DevNode::Device(DeviceKind::Zero),
            (DevNode::Root, b"random") => DevNode::Device(DeviceKind::Random),
            (DevNode::Root, b"urandom") => DevNode::Device(DeviceKind::Urandom),
            (DevNode::Root, b"tty") => DevNode::Device(DeviceKind::Tty),
            (DevNode::Root, b"console") => DevNode::Device(DeviceKind::Console),
            (DevNode::Root, b"fd") => DevNode::Link(DevLink::Fd),
            (DevNode::Root, b"stdin") => DevNode::Link(DevLink::Stdin),
            (DevNode::Root, b"stdout") => DevNode::Link(DevLink::Stdout),
            (DevNode::Root, b"stderr") => DevNode::Link(DevLink::Stderr),
            (DevNode::Dri, b".") => DevNode::Dri,
            (DevNode::Dri, b"..") => DevNode::Root,
            (DevNode::Dri, b"card0") => DevNode::Device(DeviceKind::DriCard0),
            (DevNode::Device(_) | DevNode::Link(_), _) | (DevNode::Dri, _) => {
                return Err(FileSystemError::NotFound);
            }
            (DevNode::Root, _) => return Err(FileSystemError::NotFound),
        };
        Ok(Self::new(self.filesystem_id, node)?)
    }
}

impl Inode for DevInode {
    fn filesystem_id(&self) -> usize {
        self.filesystem_id
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let device = match self.node {
            DevNode::Root | DevNode::Dri => None,
            DevNode::Device(device) => Some(device),
            DevNode::Link(_) => None,
        };
        Ok(InodeMetadata {
            filesystem: self.filesystem_id as u64,
            inode: self.node.inode(),
            kind: self.inode_type(),
            mode: self.node.mode(),
            links: if matches!(self.node, DevNode::Root) {
                2
            } else {
                1
            },
            uid: 0,
            gid: 0,
            size: match self.node {
                DevNode::Link(link) => link.target().len() as u64,
                DevNode::Root | DevNode::Dri | DevNode::Device(_) => 0,
            },
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
            DevNode::Root | DevNode::Dri => InodeType::Directory,
            DevNode::Device(_) => InodeType::CharacterDevice,
            DevNode::Link(_) => InodeType::SymLink,
        }
    }

    fn size(&self) -> u64 {
        match self.node {
            DevNode::Link(link) => link.target().len() as u64,
            DevNode::Root | DevNode::Dri | DevNode::Device(_) => 0,
        }
    }

    fn is_executable(&self) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn device_kind(&self) -> Option<DeviceKind> {
        match self.node {
            DevNode::Root | DevNode::Dri => None,
            DevNode::Device(device) => Some(device),
            DevNode::Link(_) => None,
        }
    }

    fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        match self.node {
            DevNode::Link(link) => {
                let mut target = Vec::new();
                target
                    .try_reserve_exact(link.target().len())
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                target.extend_from_slice(link.target());
                Ok(target)
            }
            DevNode::Root | DevNode::Dri | DevNode::Device(_) => {
                Err(FileSystemError::InvalidOperation)
            }
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
        let root = [
            (1, InodeType::Directory, &b"."[..]),
            (1, InodeType::Directory, &b".."[..]),
            (2, InodeType::CharacterDevice, &b"null"[..]),
            (3, InodeType::CharacterDevice, &b"zero"[..]),
            (4, InodeType::CharacterDevice, &b"tty"[..]),
            (10, InodeType::CharacterDevice, &b"random"[..]),
            (11, InodeType::CharacterDevice, &b"urandom"[..]),
            (5, InodeType::CharacterDevice, &b"console"[..]),
            (6, InodeType::SymLink, &b"fd"[..]),
            (7, InodeType::SymLink, &b"stdin"[..]),
            (8, InodeType::SymLink, &b"stdout"[..]),
            (9, InodeType::SymLink, &b"stderr"[..]),
            (12, InodeType::Directory, &b"dri"[..]),
        ];
        let dri = [
            (12, InodeType::Directory, &b"."[..]),
            (1, InodeType::Directory, &b".."[..]),
            (13, InodeType::CharacterDevice, &b"card0"[..]),
        ];
        let specifications: &[_] = match self.node {
            DevNode::Root => &root,
            DevNode::Dri => &dri,
            DevNode::Device(_) | DevNode::Link(_) => {
                return Err(FileSystemError::NotDirectory);
            }
        };
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(specifications.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for &(inode, kind, name) in specifications {
            entries.push(DirectoryEntry::try_new(inode, kind, name)?);
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

/// @description 固定设备集合的只读 devfs adapter。
pub(crate) struct DevFileSystem {
    root: Arc<DevInode>,
}

impl DevFileSystem {
    /// @description 取得标准 character nodes 与 procfs fd aliases 的唯一 device filesystem。
    pub(crate) fn instance() -> Arc<Self> {
        DEVICE_FILESYSTEM
            .call_once(|| {
                Arc::try_new(Self {
                    root: DevInode::new(DEVICE_FILESYSTEM_ID, DevNode::Root)
                        .expect("failed to allocate devfs root"),
                })
                .expect("failed to allocate devfs")
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
