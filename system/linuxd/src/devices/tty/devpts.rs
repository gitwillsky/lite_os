use alloc::{sync::Arc, vec::Vec};

use crate::fs::{
    DirectoryEntry, DirectoryRead, DirectoryVisitor, FileSystem, FileSystemError,
    FileSystemStatistics, IndexedDirectory, Inode, InodeMetadata, InodeType, SuperblockFacts,
};

use super::pty::{PtyPair, PtyPool};
use crate::fs::{
    AccessIdentity, DeviceNumber, InodeOpen, InodeOpenError, LookupOrigin, OpenedFile,
    chardev::{DeviceFile, OpenRequest},
};

struct Instance {
    pool: Arc<PtyPool>,
    uid: Option<u32>,
    gid: Option<u32>,
    mode: u32,
    ptmx_mode: u32,
    fsid: usize,
}

const DEVPTS_SUPER_MAGIC: u64 = 0x1cd1;

#[derive(Clone)]
enum DevPtsNode {
    Root,
    Master,
    Slave(Arc<PtyPair>),
}

impl DevPtsNode {
    fn inode(&self) -> u64 {
        match self {
            Self::Root => 1,
            Self::Master => 2,
            Self::Slave(pair) => pair.inode,
        }
    }
}

struct DevPtsInode {
    instance: Arc<Instance>,
    node: DevPtsNode,
}

impl DevPtsInode {
    fn new(instance: Arc<Instance>, node: DevPtsNode) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self { instance, node }).map_err(|_| FileSystemError::OutOfMemory)
    }

    fn child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        let node = match (&self.node, name) {
            (DevPtsNode::Root, b"." | b"..") => DevPtsNode::Root,
            (DevPtsNode::Root, b"ptmx") => DevPtsNode::Master,
            (DevPtsNode::Root, name) => {
                let index = parse_index(name).ok_or(FileSystemError::NotFound)?;
                DevPtsNode::Slave(
                    self.instance
                        .pool
                        .pair(index)
                        .ok_or(FileSystemError::NotFound)?,
                )
            }
            _ => return Err(FileSystemError::NotDirectory),
        };
        Ok(Self::new(self.instance.clone(), node)?)
    }

    fn open_master(
        &self,
        identity: &AccessIdentity,
    ) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        let pair = self.instance.pool.open_master(
            self.instance.uid.unwrap_or_else(|| identity.uid()),
            self.instance.gid.unwrap_or_else(|| identity.gid()),
        )?;
        Arc::try_new(super::PtyMasterFile(pair))
            .map(|file| file as Arc<dyn DeviceFile>)
            .map_err(|_| FileSystemError::OutOfMemory)
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
        self.instance.fsid
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let (kind, mode, device, uid, gid) = match &self.node {
            DevPtsNode::Root => (InodeType::Directory, 0o040755, None, 0, 0),
            DevPtsNode::Master => (
                InodeType::CharacterDevice,
                0o020000 | self.instance.ptmx_mode,
                Some(super::PTMX_NUMBER),
                0,
                0,
            ),
            DevPtsNode::Slave(pair) => (
                InodeType::CharacterDevice,
                0o020000 | self.instance.mode,
                Some(DeviceNumber::new(super::PTS_MAJOR, pair.index)),
                pair.owner_uid,
                pair.owner_gid,
            ),
        };
        Ok(InodeMetadata {
            filesystem: self.instance.fsid as u64,
            inode: self.node.inode(),
            kind,
            mode,
            links: if matches!(&self.node, DevPtsNode::Root) {
                2
            } else {
                1
            },
            uid,
            gid,
            size: 0,
            blocks: 0,
            block_size: 4096,
            atime: 0,
            mtime: 0,
            ctime: 0,
            fine_times: Default::default(),
            device,
        })
    }

    fn inode_type(&self) -> InodeType {
        match &self.node {
            DevPtsNode::Root => InodeType::Directory,
            DevPtsNode::Master | DevPtsNode::Slave(_) => InodeType::CharacterDevice,
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

    fn device_number(&self) -> Option<DeviceNumber> {
        match &self.node {
            DevPtsNode::Root => None,
            DevPtsNode::Master => Some(super::PTMX_NUMBER),
            DevPtsNode::Slave(pair) => Some(DeviceNumber::new(super::PTS_MAJOR, pair.index)),
        }
    }

    fn open_backend(
        &self,
        _opened: &OpenedFile,
        flags: u32,
        identity: &AccessIdentity,
        _authority: &crate::namespace::NamespaceAuthority,
    ) -> Result<InodeOpen, InodeOpenError> {
        let file = match &self.node {
            DevPtsNode::Root => return Ok(InodeOpen::Storage(None)),
            DevPtsNode::Master => self.open_master(identity)?,
            DevPtsNode::Slave(pair) => super::open_slave_file(pair.open_slave()?, flags)?,
        };
        Ok(InodeOpen::Device(file))
    }

    fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn read_storage(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn write_storage(
        &self,
        _offset: u64,
        _buf: &[u8],
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn append_storage(
        &self,
        _buf: &[u8],
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn truncate_storage(
        &self,
        _size: u64,
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(), FileSystemError> {
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
        if !matches!(&self.node, DevPtsNode::Root) {
            return Err(FileSystemError::NotDirectory);
        }
        let indices = self.instance.pool.live_indices()?;
        let mut stream = IndexedDirectory::new(cursor, visitor);
        if !stream.emit(
            0,
            DirectoryEntry {
                inode: 1,
                kind: InodeType::Directory,
                name: b".",
            },
        )? || !stream.emit(
            1,
            DirectoryEntry {
                inode: 1,
                kind: InodeType::Directory,
                name: b"..",
            },
        )? {
            return Ok(stream.finish());
        }
        if !stream.emit(
            2,
            DirectoryEntry {
                inode: 2,
                kind: InodeType::CharacterDevice,
                name: b"ptmx",
            },
        )? {
            return Ok(stream.finish());
        }
        let start = stream.start_index().saturating_sub(3);
        for (ordinal, (index, inode)) in indices.into_iter().enumerate().skip(start) {
            let mut storage = [0u8; 10];
            if !stream.emit(
                ordinal + 3,
                DirectoryEntry {
                    inode,
                    kind: InodeType::CharacterDevice,
                    name: index_name(index, &mut storage),
                },
            )? {
                break;
            }
        }
        Ok(stream.finish())
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.child(name)
    }

    fn create(
        &self,
        _name: &[u8],
        _kind: InodeType,
        _metadata: crate::fs::CreateMetadata,
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

/// Unix98 PTY slave namespace filesystem；节点生命周期由 pty registry 投影。
pub(crate) struct DevPtsFileSystem {
    root: Arc<DevPtsInode>,
}

impl DevPtsFileSystem {
    fn new(request: &crate::fs::MountRequest<'_>) -> Result<Arc<Self>, FileSystemError> {
        let user = request.identity.user_namespace();
        let mut limit = super::PTS_MINOR_COUNT;
        let mut uid = None;
        let mut gid = None;
        let mut mode = 0o600;
        let mut ptmx_mode = 0;
        let parse = |value: Option<&[u8]>, radix| {
            let value = core::str::from_utf8(value.ok_or(FileSystemError::InvalidOperation)?)
                .map_err(|_| FileSystemError::InvalidOperation)?;
            u32::from_str_radix(value, radix).map_err(|_| FileSystemError::InvalidOperation)
        };
        for option in crate::fs::mount_options::parse(request.options) {
            match (option.key, option.value) {
                (b"newinstance", None) => {} // 现代 Linux 的每次 devpts mount 都使用新实例。
                (b"uid", value) => {
                    uid = Some(
                        user.to_kernel(true, parse(value, 10)?)
                            .ok_or(FileSystemError::InvalidOperation)?,
                    )
                }
                (b"gid", value) => {
                    gid = Some(
                        user.to_kernel(false, parse(value, 10)?)
                            .ok_or(FileSystemError::InvalidOperation)?,
                    )
                }
                (b"mode", value) => mode = parse(value, 8)? & 0o777,
                (b"ptmxmode", value) => ptmx_mode = parse(value, 8)? & 0o777,
                (b"max", value) => limit = parse(value, 10)?.min(super::PTS_MINOR_COUNT),
                _ => return Err(FileSystemError::InvalidOperation),
            }
        }
        let instance = Arc::try_new(Instance {
            pool: PtyPool::new(limit)?,
            uid,
            gid,
            mode,
            ptmx_mode,
            fsid: crate::fs::allocate_filesystem_id(),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        Arc::try_new(Self {
            root: DevPtsInode::new(instance, DevPtsNode::Root)?,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
}

/// /dev/ptmx 的设备号入口使用实际 opened path 相邻的 pts 挂载，与 pts/ptmx 共用 master owner。
pub(super) fn open_ptmx(request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
    let parent = request
        .opened
        .device_parent()?
        .ok_or(FileSystemError::NoDevice)?;
    let pts = crate::fs::vfs()
        .open_file_at_no_follow(
            LookupOrigin::new(parent.clone(), parent),
            b"pts",
            request.identity,
        )
        .map_err(|error| match error {
            FileSystemError::NotFound | FileSystemError::NotDirectory => FileSystemError::NoDevice,
            error => error,
        })?;
    let inode = pts.inode();
    let root = (inode.as_ref() as &dyn core::any::Any)
        .downcast_ref::<DevPtsInode>()
        .filter(|inode| matches!(&inode.node, DevPtsNode::Root))
        .ok_or(FileSystemError::NoDevice)?;
    root.open_master(request.identity)
}

impl FileSystem for DevPtsFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    fn superblock(&self) -> SuperblockFacts {
        SuperblockFacts {
            type_name: "devpts",
            read_only: false,
        }
    }

    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        Ok(FileSystemStatistics {
            magic: DEVPTS_SUPER_MAGIC,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: [self.root.instance.fsid as u32, 0],
            name_length: 255,
            fragment_size: 4096,
            flags: 0,
        })
    }
}

/// devpts 的每次 mount 创建独立的 Unix98 PTY 实例。
pub(super) struct DevPtsFileSystemType;

impl crate::fs::FileSystemType for DevPtsFileSystemType {
    fn name(&self) -> &'static str {
        "devpts"
    }

    fn user_namespace_mount(&self) -> bool {
        true
    }

    fn create(
        &self,
        request: &crate::fs::MountRequest<'_>,
    ) -> Result<crate::fs::CreatedFileSystem, FileSystemError> {
        Ok(crate::fs::CreatedFileSystem {
            filesystem: DevPtsFileSystem::new(request)?,
            user_namespace: request.identity.user_namespace(),
        })
    }
}
