use alloc::{sync::Arc, vec::Vec};

use super::device::{self as registry, RegistryEntry};
use super::{
    DirectoryEntry, DirectoryRead, DirectoryVisitor, FileSystem, FileSystemError,
    FileSystemStatistics, IndexedDirectory, Inode, InodeMetadata, InodeType,
};

#[derive(Clone, Copy)]
enum DevNode {
    Root,
    Pts,
    /// 注册表隐含的目录（`directories` 下标）。
    Directory(usize),
    /// 注册表中的设备节点（`devices` 下标）。
    Registered(usize),
    Link(DevLink),
}

/// 注册节点的 inode type：按 mode 区分字符与块设备。
///
/// 只接收 mode 值：readdir 在持有注册表锁的 visitor 内调用，重新查询注册表会自旋死锁。
fn node_type(mode: u32) -> InodeType {
    const S_IFMT: u32 = 0o170000;
    const S_IFBLK: u32 = 0o060000;
    if mode & S_IFMT == S_IFBLK {
        InodeType::BlockDevice
    } else {
        InodeType::CharacterDevice
    }
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
            Self::Pts => 16,
            Self::Directory(index) => 0x200 + index as u64,
            Self::Registered(index) => 0x1000 + index as u64,
            Self::Link(DevLink::Fd) => 6,
            Self::Link(DevLink::Stdin) => 7,
            Self::Link(DevLink::Stdout) => 8,
            Self::Link(DevLink::Stderr) => 9,
        }
    }

    fn mode(self) -> u32 {
        match self {
            Self::Root | Self::Pts => 0o040755,
            Self::Directory(_) => 0o040755,
            Self::Registered(index) => registry::device(index).map_or(0, |node| node.mode),
            Self::Link(_) => 0o120777,
        }
    }

    /// 目录节点相对 `/dev` 的路径（根为空），用于注册表查找；非目录返回 `NotDirectory`。
    fn path(self) -> Result<Vec<u8>, FileSystemError> {
        let fixed: &[u8] = match self {
            Self::Root => b"",
            Self::Pts => b"pts",
            Self::Directory(index) => {
                let mut path = Vec::new();
                registry::directory_path(index, &mut path)?;
                return Ok(path);
            }
            Self::Registered(_) | Self::Link(_) => {
                return Err(FileSystemError::NotDirectory);
            }
        };
        let mut path = Vec::new();
        path.try_reserve_exact(fixed.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        path.extend_from_slice(fixed);
        Ok(path)
    }

    fn is_directory(self) -> bool {
        matches!(self, Self::Root | Self::Pts | Self::Directory(_))
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
        if let (DevNode::Directory(_), b"." | b"..") = (self.node, name) {
            let node = if name == b"." {
                self.node
            } else {
                DevNode::Root
            };
            return Ok(Self::new(self.filesystem_id, node)?);
        }
        if self.node.is_directory()
            && let Some(entry) = registry::lookup(&self.node.path()?, name)
        {
            let node = match entry {
                RegistryEntry::Directory(index) => DevNode::Directory(index),
                RegistryEntry::Device(index) => DevNode::Registered(index),
            };
            return Ok(Self::new(self.filesystem_id, node)?);
        }
        let node = match (self.node, name) {
            (DevNode::Root, b"." | b"..") => DevNode::Root,
            (DevNode::Root, b"pts") => DevNode::Pts,
            (DevNode::Root, b"fd") => DevNode::Link(DevLink::Fd),
            (DevNode::Root, b"stdin") => DevNode::Link(DevLink::Stdin),
            (DevNode::Root, b"stdout") => DevNode::Link(DevLink::Stdout),
            (DevNode::Root, b"stderr") => DevNode::Link(DevLink::Stderr),
            (DevNode::Pts, b".") => DevNode::Pts,
            (DevNode::Pts, b"..") => DevNode::Root,
            (DevNode::Registered(_) | DevNode::Link(_), _)
            | (DevNode::Pts | DevNode::Directory(_), _) => {
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
            DevNode::Root | DevNode::Pts => None,
            DevNode::Registered(index) => registry::device(index).map(|node| node.number),
            DevNode::Directory(_) | DevNode::Link(_) => None,
        };
        Ok(InodeMetadata {
            filesystem: self.filesystem_id as u64,
            inode: self.node.inode(),
            kind: self.inode_type(),
            mode: self.node.mode(),
            links: if self.node.is_directory() { 2 } else { 1 },
            uid: 0,
            gid: 0,
            size: match self.node {
                DevNode::Link(link) => link.target().len() as u64,
                DevNode::Root | DevNode::Pts | DevNode::Directory(_) | DevNode::Registered(_) => 0,
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
            DevNode::Root | DevNode::Pts | DevNode::Directory(_) => InodeType::Directory,
            DevNode::Registered(_) => node_type(self.node.mode()),
            DevNode::Link(_) => InodeType::SymLink,
        }
    }

    fn size(&self) -> u64 {
        match self.node {
            DevNode::Link(link) => link.target().len() as u64,
            DevNode::Root | DevNode::Pts | DevNode::Directory(_) | DevNode::Registered(_) => 0,
        }
    }

    fn is_executable(&self) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn device_number(&self) -> Option<super::device::DeviceNumber> {
        match self.node {
            DevNode::Registered(index) => registry::device(index).map(|node| node.number),
            _ => None,
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
            DevNode::Root | DevNode::Pts | DevNode::Directory(_) | DevNode::Registered(_) => {
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

    fn read_directory(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        let root = [
            (1, InodeType::Directory, &b"."[..]),
            (1, InodeType::Directory, &b".."[..]),
            (6, InodeType::SymLink, &b"fd"[..]),
            (7, InodeType::SymLink, &b"stdin"[..]),
            (8, InodeType::SymLink, &b"stdout"[..]),
            (9, InodeType::SymLink, &b"stderr"[..]),
            (16, InodeType::Directory, &b"pts"[..]),
        ];
        let specifications: &[_] = match self.node {
            DevNode::Root => &root,
            DevNode::Pts => &[
                (16, InodeType::Directory, &b"."[..]),
                (1, InodeType::Directory, &b".."[..]),
            ],
            DevNode::Directory(_) => &[
                (self.node.inode(), InodeType::Directory, &b"."[..]),
                (1, InodeType::Directory, &b".."[..]),
            ],
            DevNode::Registered(_) | DevNode::Link(_) => {
                return Err(FileSystemError::NotDirectory);
            }
        };
        let mut stream = IndexedDirectory::new(cursor, visitor);
        for (index, &(inode, kind, name)) in
            specifications.iter().enumerate().skip(stream.start_index())
        {
            if !stream.emit(index, DirectoryEntry { inode, kind, name })? {
                return Ok(stream.finish());
            }
        }
        // 注册表子项接在固定条目之后；注册表只追加，ordinal 在多次 getdents 间稳定。
        let parent = self.node.path()?;
        let mut ordinal = specifications.len();
        let mut result = Ok(true);
        registry::for_each_child(&parent, |entry, name, mode| {
            let index = ordinal;
            ordinal += 1;
            if index < stream.start_index() {
                return true;
            }
            let (inode, kind) = match entry {
                RegistryEntry::Directory(id) => {
                    (DevNode::Directory(id).inode(), InodeType::Directory)
                }
                RegistryEntry::Device(id) => (DevNode::Registered(id).inode(), node_type(mode)),
            };
            result = stream.emit(index, DirectoryEntry { inode, kind, name });
            matches!(result, Ok(true))
        });
        result?;
        Ok(stream.finish())
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

/// 固定设备集合的只读 devfs adapter。
pub(crate) struct DevFileSystem {
    root: Arc<DevInode>,
}

impl DevFileSystem {
    /// 创建一个 devtmpfs 视图实例；节点全部来自字符/块设备注册表，各实例内容相同。
    ///
    /// # Errors
    ///
    /// root inode 分配失败返回 `OutOfMemory`。
    pub(crate) fn new() -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            root: DevInode::new(super::allocate_filesystem_id(), DevNode::Root)?,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
}

impl FileSystem for DevFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        Ok(FileSystemStatistics {
            type_name: "devtmpfs",
            magic: 0x8584_58f6,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: [self.root.filesystem_id as u32, 0],
            name_length: 255,
            fragment_size: 4096,
            flags: 1,
        })
    }
}

/// devtmpfs 类型：不接受选项。
pub(super) struct DevFileSystemType;

impl super::mount::FileSystemType for DevFileSystemType {
    fn name(&self) -> &'static str {
        "devtmpfs"
    }

    fn create(
        &self,
        request: &super::mount::MountRequest<'_>,
    ) -> Result<Arc<dyn super::FileSystem>, FileSystemError> {
        if !super::mount_options::is_empty(request.options) {
            return Err(FileSystemError::InvalidOperation);
        }
        Ok(DevFileSystem::new()?)
    }
}
